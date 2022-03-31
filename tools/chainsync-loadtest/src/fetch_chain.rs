use std::sync::Arc;

use crate::concurrency::{Ctx, Scope};
use crate::network2 as network;
use anyhow::Context;
use log::info;
// use std::sync::atomic::Ordering;
use tokio::time;

use near_primitives::hash::CryptoHash;

// run() fetches the chain (headers,blocks and chunks)
// starting with block having hash = <start_block_hash> and
// ending with the current tip of the chain (snapshotted once
// at the start of the routine, so that the amount of work
// is bounded).
pub async fn run(
    ctx: Ctx,
    network: Arc<network::ClientManager>,
    start_block_hash: CryptoHash,
    block_limit: u64,
) -> anyhow::Result<()> {
    info!("SYNC start");
    // let peers = network.wait_for_peers(&ctx).await?;
    // let target_height = peers.iter().map(|p|p.chain_info.height).max().unwrap() as i64;
    // info!("SYNC target_height = {}", target_height);

    let start_time = time::Instant::now();
    let res = Scope::run(&ctx, {
        let network = network.clone();
        |ctx, s| async move {
            s.spawn_weak({
                let network = network.clone();
                |ctx| async move {
                    let ctx = ctx.with_label("stats");
                    loop {
                        info!("stats = {:?}", network.stats);
                        ctx.wait(time::Duration::from_secs(2)).await?;
                    }
                }
            });

            let mut last_hash = start_block_hash;
            let mut blocks_count = 0;
            loop {
                // Fetch the next batch of headers.
                let mut headers = network.any(&ctx).await?.fetch_block_headers(&ctx, &last_hash).await.context("fetch_block_headers()")?;
                headers.sort_by_key(|h| h.height());
                let last_header = headers.last().context("no headers")?;
                last_hash = last_header.hash().clone();
                info!("SYNC last_height = {:?}",last_header.height());
                for h in headers {
                    blocks_count += 1;
                    if blocks_count == block_limit {
                        return anyhow::Ok(());
                    }
                    s.spawn({
                        let network = network.clone();
                        |ctx, s| async move {
                            let block = network.any(&ctx).await?.fetch_block(&ctx, h.hash()).await.context("fetch_block()")?;
                            for ch in block.chunks().iter() {
                                let ch = ch.clone();
                                let network = network.clone();
                                s.spawn(|ctx, _s| async move {
                                    network.any(&ctx).await?.fetch_chunk(&ctx, &ch).await.context("fetch_chunk()")?;
                                    anyhow::Ok(())
                                });
                            }
                            anyhow::Ok(())
                        }
                    });
                }
            }
        }
    })
    .await;
    let stop_time = time::Instant::now();
    let total_time = stop_time - start_time;
    let _t = total_time.as_secs_f64();
    /*let sent = network.stats.msgs_sent.load(Ordering::Relaxed);
    let headers = network.stats.header_done.load(Ordering::Relaxed);
    let blocks = network.stats.block_done.load(Ordering::Relaxed);
    let chunks = network.stats.chunk_done.load(Ordering::Relaxed);
    let reqs = network.stats.peers.requests.lock().unwrap();
    let avg_latency = reqs.total_latency.as_secs_f64() / reqs.requests as f64;
    let avg_sends = (reqs.total_sends as f64) / (reqs.requests as f64);
    info!("running time: {:.2}s", t);
    info!("average QPS: {:.2}", (sent as f64) / t);
    info!("average latency: {:.2}s",avg_latency); 
    info!("average sends: {:.2}",avg_sends);
    info!("fetched {} header batches ({:.2} per second)", headers, headers as f64 / t);
    info!("fetched {} blocks ({:.2} per second)", blocks, blocks as f64 / t);
    info!("fetched {} chunks ({:.2} per second)", chunks, chunks as f64 / t);*/
    return res;
}
