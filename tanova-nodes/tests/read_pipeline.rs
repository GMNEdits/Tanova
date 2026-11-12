//! End-to-end coverage for `Read` + `DiskCache` + kernel invalidation.
//!
//! The first evaluation returns the loading gray fallback; the worker's
//! `TriggerRender` must drop the op-hash cache so the second evaluation sees
//! the real EXR pixels.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::RecvTimeoutError;
use exr::prelude::write_rgba_file;
use tanova_io::DiskCache;
use tanova_kernel::{EvalContext, KernelHandle, KernelMessage, KnobValue, NodeId, UndoableCommand};
use tanova_render::Raster2D;

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("tanova-read-pipeline-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn set_knob(node: NodeId, knob: &str, value: KnobValue, previous: KnobValue) -> KernelMessage {
    KernelMessage::Execute(UndoableCommand::SetKnob {
        node,
        knob: knob.to_string(),
        value,
        previous,
    })
}

#[test]
fn read_refreshes_after_the_disk_worker_finishes() {
    tanova_nodes::ensure_linked();

    let dir = temp_dir();
    let template = dir.join("plate.%04d.exr");
    let frame_path = dir.join("plate.0001.exr");
    write_rgba_file(&frame_path, 2, 2, |_, _| (1.0_f32, 0.0, 0.0, 1.0)).expect("write test EXR");

    let kernel = KernelHandle::spawn().expect("kernel thread should spawn");
    let node = NodeId(1);

    let mut context = EvalContext::new();
    context.insert(DiskCache::new(kernel.sender()));

    let messages = [
        KernelMessage::SetBaseContext(context),
        KernelMessage::Execute(UndoableCommand::AddNode {
            id: node,
            kind: "Read".to_string(),
        }),
        set_knob(
            node,
            "file",
            KnobValue::Text(template.to_string_lossy().into_owned()),
            KnobValue::Text(String::new()),
        ),
        set_knob(node, "first", KnobValue::Int(1), KnobValue::Int(1)),
        set_knob(node, "last", KnobValue::Int(1), KnobValue::Int(100)),
        set_knob(
            node,
            "missing_frames",
            KnobValue::Text("error".to_string()),
            KnobValue::Text("error".to_string()),
        ),
        KernelMessage::SetTime(1.0),
        KernelMessage::SetViewerNode(Some(node)),
    ];

    for message in messages {
        kernel.send(message).expect("seed Read pipeline");
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_pixel = None;
    let mut saw_red = false;

    while Instant::now() < deadline {
        match kernel.frames().recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                if let Some(raster) = frame.packet.get::<Raster2D>() {
                    let pixel = raster.sample_rgba(1, 1);
                    last_pixel = Some(pixel);
                    if pixel == [1.0, 0.0, 0.0, 1.0] {
                        saw_red = true;
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    kernel.shutdown();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        saw_red,
        "the viewer never received the loaded EXR; last pixel was {last_pixel:?}"
    );
}
