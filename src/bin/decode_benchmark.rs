#[path = "../transcription.rs"]
pub mod transcription;

use std::time::Instant;
use tokio::task;

#[tokio::main]
async fn main() {
    let mut tasks = vec![];
    let start = Instant::now();
    let ogg_bytes = std::fs::read("test_audio.ogg").unwrap();

    // Spawn 100 concurrent transcriptions
    for _ in 0..100 {
        let bytes = ogg_bytes.clone();
        tasks.push(task::spawn(async move {
            // Using the new async decode
            let _ = transcription::decode_ogg_to_f32_16khz(&bytes)
                .await
                .unwrap();
        }));
    }

    for task in tasks {
        let _ = task.await.unwrap();
    }

    let elapsed = start.elapsed();
    println!("Optimized Elapsed: {:?}", elapsed);
}
