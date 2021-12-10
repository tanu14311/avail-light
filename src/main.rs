extern crate confy;

use futures_util::{SinkExt, StreamExt};
use ipfs_embed::{Multiaddr, PeerId};
use num::{BigUint, FromPrimitive};
use std::collections::HashMap;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

mod client;
mod data;
mod http;
mod proof;
mod recovery;
mod rpc;
mod types;

#[tokio::main]
pub async fn main() {
    let cfg: types::RuntimeConfig = confy::load_path("config.yaml").unwrap();
    println!("Using {:?}", cfg);

    pub type Sto = Arc<Mutex<HashMap<u64, u32>>>;
    let db: Sto = Arc::new(Mutex::new(HashMap::new()));
    let cp = db.clone();

    // this spawns one thread of execution which runs one http server
    // for handling RPC
    let cfg_ = cfg.clone();
    thread::spawn(move || {
        http::run_server(cp.clone(), cfg_).unwrap();
    });

    // communication channels being established for talking to
    // ipfs backed application client
    let (block_tx, block_rx) = sync_channel::<types::ClientMsg>(1 << 7);
    let (self_info_tx, self_info_rx) = sync_channel::<(PeerId, Multiaddr)>(1);
    let (destroy_tx, destroy_rx) = sync_channel::<bool>(1);

    // this one will spawn one thread for running ipfs client, while managing data discovery
    // and reconstruction
    let cfg_ = cfg.clone();
    thread::spawn(move || {
        client::run_client(cfg_, block_rx, self_info_tx, destroy_rx).unwrap();
    });

    if let Ok((peer_id, addrs)) = self_info_rx.recv() {
        println!("IPFS backed application client: {}\t{:?}", peer_id, addrs);
    }

    //tokio-tungesnite method for ws connection to substrate.
    let url = url::Url::parse(&cfg.full_node_ws).unwrap();
    let (ws_stream, _response) = connect_async(url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // attempt subscription to full node block mining stream
    write
        .send(Message::Text(
            r#"{"id":1, "jsonrpc":"2.0", "method": "subscribe_newHead"}"#.to_string() + "\n",
        ))
        .await
        .unwrap();

    let _subscription_result = read.next().await.unwrap().unwrap().into_data();
    println!("Connected to Substrate Node");

    let read_future = read.for_each(|message| async {
        let data = message.unwrap().into_data();
        match serde_json::from_slice(&data) {
            Ok(response) => {
                let response: types::Response = response;
                let block_number = response.params.result.number;
                let raw = &block_number;
                let without_prefix = raw.trim_start_matches("0x");
                let z = u64::from_str_radix(without_prefix, 16);
                let num = &z.unwrap();
                let max_rows = response.params.result.extrinsics_root.rows;
                let max_cols = response.params.result.extrinsics_root.cols;
                let app_index = response.params.result.app_data_lookup.index;
                let commitment = response.params.result.extrinsics_root.commitment;

                //hyper request for getting the kate query request
                let cells =
                    rpc::get_kate_proof(&cfg.full_node_rpc, *num, max_rows, max_cols, false)
                        .await
                        .unwrap();
                println!("Verifying block {}", *num);

                //hyper request for verifying the proof
                let count = proof::verify_proof(max_rows, max_cols, &cells, &commitment);
                println!(
                    "Completed {} rounds of verification for block number {} ",
                    count, num
                );

                let conf = calculate_confidence(count);
                let serialised_conf = serialised_confidence(*num, conf);
                {
                    let mut handle = db.lock().unwrap();
                    handle.insert(*num, count);
                    println!(
                        "block: {}, confidence: {}, serialisedConfidence {}",
                        *num, conf, serialised_conf
                    );
                }

                /*note:
                The following is the part when the user have already subscribed
                to an appID and now its verifying every cell that contains the data
                */
                if !app_index.is_empty() {
                    let req_id = cfg.app_id;
                    if conf > 92.0 && req_id > 0 {
                        let req_cells =
                            rpc::get_kate_proof(&cfg.full_node_rpc, *num, max_rows, max_cols, true)
                                .await
                                .unwrap();
                        println!("Verifying block :{} because APPID is given ", *num);
                        //hyper request for verifying the proof
                        let count =
                            proof::verify_proof(max_rows, max_cols, &req_cells, &commitment);
                        println!(
                            "Completed {} rounds of verification for block number {} ",
                            count, num
                        );
                    }
                }

                // notify ipfs-based application client
                // that newly mined block has been received
                block_tx
                    .send(types::ClientMsg {
                        num: *num,
                        max_rows: max_rows,
                        max_cols: max_cols,
                    })
                    .unwrap();
            }
            Err(error) => println!("Misconstructed Header: {:?}", error),
        }
    });

    read_future.await;
    // inform ipfs-backed application client running thread
    // that it can kill self now, as process is going to die itself !
    destroy_tx.send(true).unwrap();
}

/* note:
    following are the support functions.
*/
pub fn fill_cells_with_proofs(cells: &mut Vec<types::Cell>, proof: &types::BlockProofResponse) {
    assert_eq!(80 * cells.len(), proof.result.len());
    for i in 0..cells.len() {
        let mut v = Vec::new();
        v.extend_from_slice(&proof.result[i * 80..i * 80 + 80]);
        cells[i].proof = v;
    }
}

fn calculate_confidence(count: u32) -> f64 {
    100f64 * (1f64 - 1f64 / 2u32.pow(count) as f64)
}

fn serialised_confidence(block: u64, factor: f64) -> String {
    let _block: BigUint = FromPrimitive::from_u64(block).unwrap();
    let _factor: BigUint = FromPrimitive::from_u64((10f64.powi(7) * factor) as u64).unwrap();
    let _shifted: BigUint = _block << 32 | _factor;
    _shifted.to_str_radix(10)
}
