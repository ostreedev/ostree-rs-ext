//! APIs for extracting OSTree commits from container images

use super::oci;
use super::Result;
use anyhow::{anyhow, Context};
use fn_error_context::context;
use hyper::body::Body;
use std::convert::TryInto;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::result::Result as StdResult;

/// Reinterpret a TCP socket as a File.
/// Generally the only safe use of this is to then re-interpret the File as a RawFd later.
#[allow(unsafe_code)]
fn reinterpret_to_file<S>(s: S) -> File
where
    S: IntoRawFd,
{
    unsafe { File::from_raw_fd(s.into_raw_fd()) }
}

struct PodmanImageProxy {
    tempdir: tempfile::TempDir,
    proc: subprocess::Popen,
    sender: hyper::client::conn::SendRequest<Body>,
    conn: tokio::task::JoinHandle<StdResult<(), hyper::Error>>,
}

impl PodmanImageProxy {
    #[context("Creating podman proxy")]
    async fn new() -> Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let (mysock, childsock) = UnixStream::pair()?;
        let podman_path = std::env::var_os("PODMAN_PATH").unwrap_or_else(|| "podman".into());
        let proc = subprocess::Exec::cmd("setpriv")
            .args(&["--pdeathsig", "SIGTERM", "--"])
            .arg(podman_path)
            .arg("--root")
            .arg(tempdir.path())
            .args(&["local-image-proxy", "--sockfd", "0"])
            .stdin(subprocess::Redirection::File(reinterpret_to_file(
                childsock,
            )))
            .popen()
            .context("Failed to spawn podman")?;
        let mysock = tokio::net::UnixStream::from_std(mysock)?;
        let connbuilder = hyper::client::conn::Builder::new();
        let (sender, conn) = connbuilder.handshake(mysock).await?;
        let conn = tokio::spawn(conn);
        Ok(Self {
            proc,
            sender,
            conn,
            tempdir,
        })
    }

    #[context("Fetching manifest")]
    async fn fetch_manifest(
        &mut self,
        imgref: &oci_distribution::Reference,
    ) -> Result<(oci::Manifest, String)> {
        let req = hyper::Request::builder()
            .uri(format!(
                "{}/{}/manifests/{}",
                imgref.registry(),
                imgref.repository(),
                imgref.tag().unwrap_or("latest")
            ))
            .header(hyper::header::HOST, "podman.com")
            .body(Body::empty()).context("Creating request")?;
        let res = self.sender.send_request(req).await.context("Failed to send request")?;
        let res = hyper::body::to_bytes(res).await?;
        let digest = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), &res)?;
        Ok((
            serde_json::from_slice(&res)?,
            format!("sha256:{}", hex::encode(digest)),
        ))
    }

    #[context("Fetching blob")]
    async fn fetch_blob(
        &mut self,
        imgref: &oci_distribution::Reference,
        blob: &str,
    ) -> Result<Body> {
        let req = hyper::Request::builder()
            .uri(format!(
                "{}/{}/blobs/{}",
                imgref.registry(),
                imgref.repository(),
                blob,
            ))
            .header(hyper::header::HOST, "podman.com")
            .body(Body::empty())?;
        Ok(self.sender.send_request(req).await?.into_body())
    }

    async fn close(mut self) {
        let _ = self.proc.kill();
        let _ = self.conn.await;
        drop(self.tempdir);
    }
}

/// The result of an import operation
#[derive(Debug)]
pub struct Import {
    /// The ostree commit that was imported
    pub ostree_commit: String,
    /// The image digest retrieved
    pub image_digest: String,
}

fn find_layer_descriptor(manifest: &oci::Manifest) -> Result<&oci::ManifestLayer> {
    let layers: Vec<_> = manifest
        .layers
        .iter()
        .filter(|&layer| {
            matches!(
                layer.media_type.as_str(),
                super::oci::DOCKER_TYPE_LAYER | oci::OCI_TYPE_LAYER
            )
        })
        .collect();

    let n = layers.len();
    if let Some(layer) = layers.into_iter().next() {
        if n > 1 {
            Err(anyhow!("Expected 1 layer, found {}", n))
        } else {
            Ok(layer)
        }
    } else {
        Err(anyhow!("No layers found (orig: {})", manifest.layers.len()))
    }
}

#[allow(unsafe_code)]
#[context("Importing {}", imgref)]
async fn import_impl(repo: &ostree::Repo, imgref: &str) -> Result<Import> {
    let imgref: oci_distribution::Reference = imgref
        .try_into()
        .context("Failed to parse image reference")?;
    let mut client = PodmanImageProxy::new().await?;
    let (manifest, image_digest) = client.fetch_manifest(&imgref).await?;
    let manifest = &manifest;
    let layerid = find_layer_descriptor(manifest)?;
    let layer = client.fetch_blob(&imgref, &layerid.digest).await?;
    let (pipein, mut pipeout) = os_pipe::pipe()?;
    let copier = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let req = futures::executor::block_on_stream(layer);
        for v in req {
            let v = v.map_err(anyhow::Error::msg).context("Writing buf")?;
            pipeout.write_all(&v)?;
        }
        Ok(())
    });
    let repo = repo.clone();
    let import = tokio::task::spawn_blocking(move || {
        let gz = flate2::read::GzDecoder::new(pipein);
        crate::tar::import_tar(&repo, gz)
    });
    let (import_res, copy_res) = tokio::join!(import, copier);
    copy_res??;
    let ostree_commit = import_res??;

    client.close().await;

    Ok(Import {
        ostree_commit,
        image_digest,
    })
}

/// Download and import the referenced container
pub async fn import<I: AsRef<str>>(repo: &ostree::Repo, image_ref: I) -> Result<Import> {
    Ok(import_impl(repo, image_ref.as_ref()).await?)
}
