//! Atomic observations with awaited Unix datagram notifications.

use std::path::Path;

use tokio::net::UnixDatagram;
use uuid::Uuid;

pub(super) async fn checkpoint(directory: &Path, name: &str, bytes: &[u8]) {
    let temporary = directory.join(format!("{name}.{}.tmp", Uuid::now_v7()));
    tokio::fs::write(&temporary, bytes).await.unwrap();
    tokio::fs::rename(temporary, directory.join(name))
        .await
        .unwrap();
    UnixDatagram::unbound()
        .unwrap()
        .send_to(name.as_bytes(), directory.join("events.sock"))
        .await
        .unwrap();
}

// Mockito's request callback is synchronous. This notification happens after
// applying the commit, before allowing any response bytes back to the publisher.
pub(super) fn checkpoint_sync(directory: &Path, name: &str) {
    let temporary = directory.join(format!("{name}.{}.tmp", Uuid::now_v7()));
    std::fs::write(&temporary, []).unwrap();
    std::fs::rename(temporary, directory.join(name)).unwrap();
    std::os::unix::net::UnixDatagram::unbound()
        .unwrap()
        .send_to(name.as_bytes(), directory.join("events.sock"))
        .unwrap();
}
