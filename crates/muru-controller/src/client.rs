//! The admin API client, for muructl: a gRPC channel over the daemon's
//! Unix socket.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use muru_proto::admin::v1::admin_client::AdminClient;
use tonic::transport::{Channel, Endpoint, Uri};

/// The largest reply the client takes, in MiB. A `Debug` reply holds
/// every worker's answer in one message, so tonic's default of 4 MiB is
/// too small, and a limit remains so that a reply cannot take all of the
/// client's memory. The servers set no encoding limit.
pub const MAX_REPLY_MIB: usize = 256;

/// Connect to the daemon's admin socket.
pub async fn connect(socket: &Path) -> anyhow::Result<AdminClient<Channel>> {
    let socket: PathBuf = socket.to_path_buf();
    let found = std::fs::metadata(&socket);

    if found
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    {
        anyhow::bail!(permission_denied(&socket));
    }

    if found.is_err() {
        anyhow::bail!(nothing_listening(&socket));
    }

    // The URI is required by the channel but never used: the connector
    // opens the Unix socket whatever it says.

    let endpoint = Endpoint::try_from("http://muru.invalid")?;
    let dialled = socket.clone();
    let channel = endpoint
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket = dialled.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .map_err(|error| {
            if caused_by(&error, std::io::ErrorKind::PermissionDenied) {
                anyhow::anyhow!(permission_denied(&socket))
            } else if caused_by(&error, std::io::ErrorKind::ConnectionRefused) {
                anyhow::anyhow!(nothing_listening(&socket))
            } else {
                anyhow::Error::from(error)
            }
        })?;

    Ok(AdminClient::new(channel).max_decoding_message_size(MAX_REPLY_MIB * 1024 * 1024))
}

/// Whether `socket` refuses connections: a file left behind by something
/// that has gone. A socket that answers, or that fails in another way
/// such as a permission error, is not one.
pub fn left_behind(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused)
}

/// Whether a failed connection came down to this kind of error, somewhere
/// in the transport's chain of causes.
fn caused_by(error: &(dyn std::error::Error + 'static), kind: std::io::ErrorKind) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == kind)
    })
}

/// What to say when no daemon or development run is at the admin socket.
fn nothing_listening(socket: &Path) -> String {
    format!(
        "nothing is listening at {}: no daemon (muructl daemon start, or muructl up) and no \
         development run (muructl run)",
        socket.display()
    )
}

/// What to say when the admin socket may not be used: the socket and who
/// owns it, or its directory's owner when the socket cannot be looked at.
fn permission_denied(socket: &Path) -> String {
    let owner = std::fs::metadata(socket)
        .map(|found| format!("it belongs to UID {}", found.uid()))
        .or_else(|_| {
            socket
                .parent()
                .map_or(Err(std::io::ErrorKind::NotFound.into()), std::fs::metadata)
                .map(|found| format!("its directory belongs to UID {}", found.uid()))
        })
        .unwrap_or_else(|_: std::io::Error| "its owner cannot be read".to_owned());

    format!("permission denied on {}: {owner}", socket.display())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[tokio::test]
    async fn a_socket_left_behind_is_told_from_one_listening() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("admin.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        assert!(!left_behind(&path));

        // Closing the listener leaves the socket's file where it was.

        drop(listener);
        assert!(left_behind(&path));

        let error = connect(&path).await.expect_err("nothing is listening");
        assert_eq!(error.to_string(), nothing_listening(&path));
    }

    #[tokio::test]
    async fn a_socket_that_may_not_be_used_is_named_with_its_owner() {
        // SAFETY: geteuid has no preconditions and cannot fail.

        let uid = unsafe { libc::geteuid() };

        // Root is not held to a socket's mode, so the case cannot arise.

        if uid == 0 {
            return;
        }

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("admin.sock");
        let _listener = tokio::net::UnixListener::bind(&path).expect("bind");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let error = connect(&path)
            .await
            .expect_err("the socket may not be used");
        assert_eq!(
            error.to_string(),
            format!(
                "permission denied on {}: it belongs to UID {uid}",
                path.display()
            )
        );
    }
}
