//! APIs for extracting OSTree commits from container images

use super::oci;
use super::Result;
use anyhow::{anyhow, Context};
use fn_error_context::context;
use futures::{Future, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use std::convert::TryFrom;
use std::convert::TryInto;
use std::io::prelude::*;
use std::os::unix::io::{AsRawFd, IntoRawFd};
use std::process::Stdio;
use std::rc::Rc;
use tokio::{io::AsyncRead, process::Command};

fn new_skopeo() -> tokio::process::Command {
    let mut cmd = Command::new("skopeo");
    cmd.kill_on_drop(true);
    cmd
}

fn skopeo_spawn(mut cmd: Command) -> Result<tokio::process::Child> {
    let cmd = cmd.stdin(Stdio::null()).stderr(Stdio::piped());
    Ok(cmd.spawn().context("Failed to exec skopeo")?)
}

fn skopeo_ref(imgref: &oci_distribution::Reference) -> String {
    format!("docker://{}", imgref)
}

#[context("Fetching manifest")]
async fn fetch_manifest(imgref: &oci_distribution::Reference) -> Result<(oci::Manifest, String)> {
    let mut proc = new_skopeo();
    proc.args(&["inspect", "--raw"]).arg(skopeo_ref(imgref));
    proc.stdout(Stdio::piped());
    let proc = skopeo_spawn(proc)?.wait_with_output().await?;
    if !proc.status.success() {
        let errbuf = String::from_utf8_lossy(&proc.stderr);
        return Err(anyhow!("skopeo inspect failed\n{}", errbuf));
    }
    let raw_manifest = proc.stdout;
    let digest = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), &raw_manifest)?;
    let digest = format!("sha256:{}", hex::encode(&digest));
    Ok((serde_json::from_slice(&raw_manifest)?, digest))
}

#[allow(unsafe_code)]
/// Given a file descriptor backed object, set up the child process to use
/// it o
fn tokio_command_take_fd<F>(cmd: &mut Command, fd: F, target: libc::c_int)
where
    F: AsRawFd + Send + Sync + 'static,
{
    unsafe {
        cmd.pre_exec(move || {
            nix::unistd::dup2(fd.as_raw_fd(), target)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
                .map(|_| {})
        });
    }
}

/// Fetch the whole content of a container image into OCI Archive format, which is
/// just a tarball of the manifest and layers, streaming the result.
async fn fetch_oci_archive(
    imgref: &oci_distribution::Reference,
) -> Result<(impl AsyncRead, impl Future<Output = Result<()>>)> {
    let mut proc = new_skopeo();
    proc.stdout(Stdio::null());
    let target_fd = 5i32;
    let (pipein, pipeout) = os_pipe::pipe()?;
    // TODO add api to tokio to pass file descriptors
    proc.arg("copy")
        .arg(skopeo_ref(imgref))
        .arg(format!("oci-archive:///proc/self/fd/{}", target_fd));
    tokio_command_take_fd(&mut proc, pipeout, target_fd);
    let proc = proc.status().err_into().and_then(|e| async move {
        if !e.success() {
            return Err(anyhow!("skopeo failed: {}", e));
        }
        Ok(())
    });
    // FIXME this leaks the pipefd on error
    let pipein = tokio_fd::AsyncFd::try_from(pipein.into_raw_fd())?;
    Ok((pipein, proc))
}

fn read_oci_archive_blob(
    archive: impl AsyncRead + Send + Unpin + 'static,
    blobid: &str,
) -> Result<(
    impl Future<Output = Result<super::asynctar::TarEntry>>,
    impl Future<Output = Result<()>>,
)> {
    let blobpath = Rc::new(format!("blobs/sha256/{}", blobid));
    let (s, worker) = super::asynctar::parse_tar(archive)?;
    let blobpath_copy = Rc::clone(&blobpath);
    let blob = s
        .try_filter_map(move |elt| {
            let blobpath = Rc::clone(&blobpath_copy);
            async move {
                if elt.header.path()?.to_str() == Some(blobpath.as_str()) {
                    Ok(Some(elt))
                } else {
                    Ok(None)
                }
            }
        })
        .boxed_local()
        .into_future()
        .then(|(first, _)| async move {
            first.ok_or_else(|| anyhow!("Couldn't find entry"))?
        });
    Ok((blob.boxed_local(), worker))
}

/// The result of an import operation
#[derive(Debug)]
pub struct Import {
    /// The ostree commit that was imported
    pub ostree_commit: String,
    /// The image digest retrieved
    pub image_digest: String,
}

fn find_layer_blobid(manifest: &oci::Manifest) -> Result<String> {
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
            let digest = layer.digest.as_str();
            let hash = digest
                .strip_prefix("sha256:")
                .ok_or_else(|| anyhow!("Expected sha256: in digest: {}", digest))?;
            Ok(hash.into())
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
    let (manifest, image_digest) = fetch_manifest(&imgref).await?;
    let manifest = &manifest;
    let layerid = find_layer_blobid(manifest)?;
    let (archive_in, fetch_worker) = fetch_oci_archive(&imgref).await?;
    let (blob, parse_worker) = read_oci_archive_blob(archive_in, layerid.as_str())?;
    let blob = blob.await?;
    let (pipein, mut pipeout) = os_pipe::pipe()?;
    let copier = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut content = blob
            .content
            .ok_or_else(|| anyhow!("Blob layer is not a regular file"))?;
        while let Some(buf) = content.blocking_recv() {
            let buf: bytes::Bytes = buf;
            pipeout.write_all(&buf)?;
        }
        Ok(())
    });
    let repo = repo.clone();
    let import = tokio::task::spawn_blocking(move || {
        let gz = flate2::read::GzDecoder::new(pipein);
        crate::tar::import_tar(&repo, gz)
    });
    let (import_res, copy_res, fetch_worker, parse_worker) = tokio::join!(import, copier, fetch_worker, parse_worker);
    dbg!(&import_res, &copy_res, &fetch_worker, &parse_worker);
    fetch_worker?;
    parse_worker?;
    copy_res??;
    let ostree_commit = import_res??;

    Ok(Import {
        ostree_commit,
        image_digest,
    })
}

/// Download and import the referenced container
pub async fn import<I: AsRef<str>>(repo: &ostree::Repo, image_ref: I) -> Result<Import> {
    Ok(import_impl(repo, image_ref.as_ref()).await?)
}
