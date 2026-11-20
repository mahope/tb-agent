use serde_json::{Value, json};
use tb_protocol::Frame;

use crate::{FakeConfig, result_frames};

#[test]
fn large_results_split_into_ordered_chunks_that_rebuild_the_result() {
    let config = FakeConfig {
        stream_threshold: 10,
        chunk_size: 7,
        ..FakeConfig::default()
    };
    let result = json!({"body": "aé".repeat(20)});
    let frames = result_frames("1", &result, &config);
    assert!(frames.len() > 3);
    let mut text = String::new();
    for (i, f) in frames.iter().enumerate() {
        match f {
            Frame::Chunk { seq, data, .. } => {
                assert_eq!(*seq as usize, i);
                text.push_str(data);
            }
            Frame::Response {
                streamed, result, ..
            } => {
                assert!(*streamed && result.is_none());
                assert_eq!(i, frames.len() - 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), result);
}
