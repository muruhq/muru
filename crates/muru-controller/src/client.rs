//! The admin API client, for muructl: a gRPC channel over the daemon's
//! Unix socket.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use muru_proto::admin::v1::admin_client::AdminClient;
use tonic::transport::{Channel, Endpoint, Uri};

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
        anyhow::bail!(
            "nothing is listening at {}: no daemon (muructl daemon start, or muructl up) and no \
             development run (muructl run)",
            socket.display()
        );
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
            if refused_permission(&error) {
                anyhow::anyhow!(permission_denied(&socket))
            } else {
                anyhow::Error::from(error)
            }
        })?;

    Ok(AdminClient::new(channel))
}

/// Whether a failed connection came down to a permission error, somewhere
/// in the transport's chain of causes.
fn refused_permission(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
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
