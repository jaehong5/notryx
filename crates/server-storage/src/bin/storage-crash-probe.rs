use notryx_server_storage::{FileStore, StorageStage};
use std::io::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 4 {
        return Err("usage: storage-crash-probe NOTES STATE OPERATION STAGE".into());
    }
    let store = FileStore::open(&arguments[0], &arguments[1])?;
    let operation = arguments[2].to_str().ok_or("invalid operation")?;
    let selected_stage = arguments[3].to_str().ok_or("invalid stage")?;
    let hook = |stage: StorageStage| {
        if format!("{stage:?}") == selected_stage {
            println!("STORAGE_CUT_REACHED");
            std::io::stdout().flush().expect("flush test barrier");
            loop {
                std::thread::park();
            }
        }
        Ok(())
    };
    match operation {
        "upload" => {
            let mut writer = store.begin_upload(uuid::Uuid::new_v4(), 8)?;
            writer.write_chunk(b"proposed")?;
            store.commit_upload_with_hook(writer.seal()?, hook)?;
        }
        "object" => {
            store.put_object_with_hook(b"proposed", hook)?;
        }
        "replace" => {
            let observed = store.observe("document.md")?;
            let proposed = store.put_object(b"proposed")?;
            store.replace_with_hook("document.md", &observed.stamp, &proposed, hook)?;
        }
        "recovery" => {
            let observed = store.observe("document.md")?;
            store.sync_observed_with_hook("document.md", &observed.stamp, hook)?;
        }
        _ => return Err("invalid operation".into()),
    }
    Err("test barrier was not reached".into())
}
