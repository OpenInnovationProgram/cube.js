use crate::config::ConfigObj;
use crate::di_service;
use crate::remotefs::{RemoteFile, RemoteFs};
use crate::util::lock::acquire_lock;
use crate::CubeError;
use async_trait::async_trait;
use core::fmt;
use datafusion::cube_ext;
use deadqueue::unlimited;
use futures::future::join_all;
use log::error;
use smallvec::alloc::fmt::Formatter;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fs::Metadata;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, watch, RwLock};
use tokio::time::Duration;

pub struct QueueRemoteFs {
    config: Arc<dyn ConfigObj>,
    remote_fs: Arc<dyn RemoteFs>,
    upload_queue: unlimited::Queue<RemoteFsOp>,
    download_queue: unlimited::Queue<RemoteFsOp>,
    // TODO not used
    deleted: RwLock<HashSet<String>>,
    downloading: RwLock<HashSet<String>>,
    _result_receiver: broadcast::Receiver<RemoteFsOpResult>,
    result_sender: broadcast::Sender<RemoteFsOpResult>,
    stopped_rx: watch::Receiver<bool>,
    stopped_tx: watch::Sender<bool>,
}

impl Debug for QueueRemoteFs {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueRemoteFs")
            .field("remote_fs", &self.remote_fs)
            .finish()
    }
}

#[derive(Debug)]
pub enum RemoteFsOp {
    Upload {
        temp_upload_path: String,
        remote_path: String,
    },
    Delete(String),
    Download(String, Option<u64>),
}

#[derive(Debug, Clone)]
pub enum RemoteFsOpResult {
    Upload(String, Result<u64, CubeError>),
    Delete(String, Result<(), CubeError>),
    Download(String, Result<String, CubeError>),
}

di_service!(QueueRemoteFs, [RemoteFs]);

impl QueueRemoteFs {
    pub fn new(config: Arc<dyn ConfigObj>, remote_fs: Arc<dyn RemoteFs>) -> Arc<Self> {
        let (stopped_tx, stopped_rx) = watch::channel(false);
        let (tx, rx) = broadcast::channel(16384);
        Arc::new(Self {
            config,
            remote_fs,
            upload_queue: unlimited::Queue::new(),
            download_queue: unlimited::Queue::new(),
            deleted: RwLock::new(HashSet::new()),
            downloading: RwLock::new(HashSet::new()),
            result_sender: tx,
            _result_receiver: rx,
            stopped_tx,
            stopped_rx,
        })
    }

    pub async fn wait_processing_loops(queue_remote_fs: Arc<Self>) -> Result<(), CubeError> {
        let mut futures = Vec::new();
        for _ in 0..queue_remote_fs.config.upload_concurrency() {
            let to_move = queue_remote_fs.clone();
            futures.push(cube_ext::spawn(async move {
                let mut stopped_rx = to_move.stopped_rx.clone();
                loop {
                    let to_process = tokio::select! {
                        to_process = to_move.upload_queue.pop() => {
                            to_process
                        }
                        res = stopped_rx.changed() => {
                            if res.is_err() || *stopped_rx.borrow() {
                                return;
                            }
                            continue;
                        }
                    };

                    if let Err(err) = to_move.upload_loop(to_process).await {
                        error!("Error during upload: {:?}", err);
                    }
                }
            }));
        }

        for _ in 0..queue_remote_fs.config.download_concurrency() {
            let to_move = queue_remote_fs.clone();
            futures.push(cube_ext::spawn(async move {
                let mut stopped_rx = to_move.stopped_rx.clone();
                loop {
                    let to_process = tokio::select! {
                        to_process = to_move.download_queue.pop() => {
                            to_process
                        }
                        res = stopped_rx.changed() => {
                            if res.is_err() || *stopped_rx.borrow() {
                                return;
                            }
                            continue;
                        }
                    };

                    if let Err(err) = to_move.download_loop(to_process).await {
                        error!("Error during download: {:?}", err);
                    }
                }
            }));
        }

        let to_move = queue_remote_fs.clone();
        if queue_remote_fs.config.upload_to_remote() {
            futures.push(tokio::task::spawn(async move {
                to_move.cleanup_loop().await;
            }));
        }
        join_all(futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(())
    }

    pub fn stop_processing_loops(&self) -> Result<(), CubeError> {
        Ok(self.stopped_tx.send(true)?)
    }

    async fn upload_loop(&self, to_process: RemoteFsOp) -> Result<(), CubeError> {
        match to_process {
            RemoteFsOp::Upload {
                temp_upload_path,
                remote_path,
            } => {
                if !acquire_lock("upload loop deleted", self.deleted.read())
                    .await?
                    .contains(remote_path.as_str())
                {
                    let mut res = self
                        .remote_fs
                        .upload_file(&temp_upload_path, &remote_path)
                        .await;
                    if let Ok(size) = res {
                        match self.remote_fs.list_with_metadata(&remote_path).await {
                            Ok(list) => {
                                let list_res = list.iter().next().ok_or(CubeError::internal(
                                    format!("File {} can't be listed after upload", remote_path),
                                ));
                                match list_res {
                                    Ok(file) => {
                                        if file.file_size != size {
                                            res = Err(CubeError::internal(format!(
                                                "File sizes for {} doesn't match after upload. Expected to be {} but {} uploaded",
                                                remote_path,
                                                size,
                                                file.file_size
                                            )));
                                        }
                                    }
                                    Err(e) => {
                                        res = Err(e);
                                    }
                                }
                            }
                            Err(e) => {
                                res = Err(e);
                            }
                        }
                    }
                    self.result_sender
                        .send(RemoteFsOpResult::Upload(remote_path, res))?;
                }
            }
            RemoteFsOp::Delete(file) => {
                self.result_sender.send(RemoteFsOpResult::Delete(
                    file.to_string(),
                    self.remote_fs.delete_file(file.as_str()).await,
                ))?;
            }
            x => panic!("Unexpected operation: {:?}", x),
        }
        Ok(())
    }

    async fn download_loop(&self, to_process: RemoteFsOp) -> Result<(), CubeError> {
        match to_process {
            RemoteFsOp::Download(file, expected_file_size) => {
                let result = self
                    .remote_fs
                    .download_file(file.as_str(), expected_file_size)
                    .await;
                let mut downloading =
                    acquire_lock("download loop downloading", self.downloading.write()).await?;
                self.result_sender
                    .send(RemoteFsOpResult::Download(file.to_string(), result))?;
                downloading.remove(&file);
            }
            x => panic!("Unexpected operation: {:?}", x),
        }
        Ok(())
    }

    const CLEANUP_INTERVAL: Duration = Duration::from_secs(600);
    /// Periodically cleans up the local directory from the files removed on the remote side.
    /// This function currently removes only direct sibling files and does not touch subdirectories.
    /// So e.g. we remove the `.parquet` files, but not directories like `metastore` or heartbeat.
    ///
    /// Uploads typically live in the `uploads` directory while being prepared and only get moved
    /// to be direct siblings **after** appearing on the server.
    async fn cleanup_loop(&self) -> () {
        let local_dir = self.local_path().await;
        let mut stopped_rx = self.stopped_rx.clone();
        loop {
            // Do the cleanup every now and then.
            tokio::select! {
                () = tokio::time::sleep(Self::CLEANUP_INTERVAL) => {},
                res = stopped_rx.changed() => {
                    if res.is_err() || *stopped_rx.borrow() {
                        return;
                    }
                }
            }

            // Important to collect local files **before** remote to avoid invalid removals.
            // We rely on RemoteFs implementations to upload the file to the server before they make
            // it available on the local filesystem.
            let local_dir_copy = local_dir.clone();
            let res_local_files =
                cube_ext::spawn_blocking(move || -> Result<HashSet<String>, std::io::Error> {
                    let mut local_files = HashSet::new();
                    for res_entry in Path::new(&local_dir_copy).read_dir()? {
                        let entry = match res_entry {
                            Err(_) => continue, // ignore errors, might come from concurrent fs ops.
                            Ok(e) => e,
                        };

                        let ft = match entry.file_type() {
                            Err(_) => continue,
                            Ok(ft) => ft,
                        };
                        if !ft.is_file() {
                            continue;
                        }

                        let file_name = match entry.file_name().into_string() {
                            Err(_) => {
                                log::error!("could not convert file name {:?}", entry.file_name());
                                continue;
                            }
                            Ok(name) => name,
                        };

                        local_files.insert(file_name);
                    }
                    Ok(local_files)
                })
                .await
                .unwrap();

            let mut local_files = match res_local_files {
                Err(e) => {
                    log::error!("error while trying to list local files: {}", e);
                    continue;
                }
                Ok(f) => f,
            };

            let res_remote_files = self.list("").await;
            let remote_files = match res_remote_files {
                Err(e) => {
                    log::error!("could not get the list of remote files: {}", e);
                    continue;
                }
                Ok(f) => f,
            };

            // Only keep the files we want to remove in `local_files`.
            for f in remote_files {
                local_files.remove(&f);
            }

            if !local_files.is_empty() {
                log::debug!(
                    "Cleaning up {} files that were removed remotely",
                    local_files.len()
                );
                log::trace!("The files being removed are {:?}", local_files);
            }

            let local_dir_copy = local_dir.clone();
            cube_ext::spawn_blocking(move || {
                for f in local_files {
                    let _ = std::fs::remove_file(Path::new(&local_dir_copy).join(f));
                }
            })
            .await
            .unwrap();
        }
    }
}

#[async_trait]
impl RemoteFs for QueueRemoteFs {
    async fn upload_file(
        &self,
        local_upload_path: &str,
        remote_path: &str,
    ) -> Result<u64, CubeError> {
        if !self.config.upload_to_remote() {
            log::info!("Skipping upload {}", remote_path);
            return Ok(tokio::fs::metadata(local_upload_path).await?.len());
        }
        let mut receiver = self.result_sender.subscribe();
        self.upload_queue.push(RemoteFsOp::Upload {
            temp_upload_path: local_upload_path.to_string(),
            remote_path: remote_path.to_string(),
        });
        loop {
            let res = receiver.recv().await?;
            if let RemoteFsOpResult::Upload(file, result) = res {
                if &file == remote_path {
                    return result;
                }
            }
        }
    }

    async fn download_file(
        &self,
        remote_path: &str,
        expected_file_size: Option<u64>,
    ) -> Result<String, CubeError> {
        // We might be lucky and the file has already been downloaded.
        if let Ok(local_path) = self.local_file(remote_path).await {
            let metadata = tokio::fs::metadata(&local_path).await;
            if metadata.is_ok() {
                if let Err(e) = QueueRemoteFs::check_file_size(
                    remote_path,
                    expected_file_size,
                    &local_path,
                    metadata.unwrap(),
                )
                .await
                {
                    return Err(e);
                }
                return Ok(local_path);
            }
        }
        let mut receiver = self.result_sender.subscribe();
        {
            let mut downloading =
                acquire_lock("download file downloading", self.downloading.write()).await?;
            if !downloading.contains(remote_path) {
                self.download_queue.push(RemoteFsOp::Download(
                    remote_path.to_string(),
                    expected_file_size,
                ));
                downloading.insert(remote_path.to_string());
            }
        }
        loop {
            let res = receiver.recv().await?;
            if let RemoteFsOpResult::Download(file, result) = res {
                if &file == remote_path {
                    let local_path = self.local_file(remote_path).await?;
                    let metadata = tokio::fs::metadata(&local_path).await?;
                    if let Err(e) = QueueRemoteFs::check_file_size(
                        remote_path,
                        expected_file_size,
                        &local_path,
                        metadata,
                    )
                    .await
                    {
                        return Err(e);
                    }
                    return result;
                }
            }
        }
    }

    async fn delete_file(&self, remote_path: &str) -> Result<(), CubeError> {
        if !self.config.upload_to_remote() {
            log::info!("Skipping delete {}", remote_path);
            return Ok(());
        }
        let mut receiver = self.result_sender.subscribe();
        self.upload_queue
            .push(RemoteFsOp::Delete(remote_path.to_string()));
        loop {
            let res = receiver.recv().await?;
            if let RemoteFsOpResult::Delete(file, result) = res {
                if &file == remote_path {
                    return result;
                }
            }
        }
    }

    async fn list(&self, remote_prefix: &str) -> Result<Vec<String>, CubeError> {
        self.remote_fs.list(remote_prefix).await
    }

    async fn list_with_metadata(&self, remote_prefix: &str) -> Result<Vec<RemoteFile>, CubeError> {
        self.remote_fs.list_with_metadata(remote_prefix).await
    }

    async fn local_path(&self) -> String {
        self.remote_fs.local_path().await
    }

    async fn local_file(&self, remote_path: &str) -> Result<String, CubeError> {
        self.remote_fs.local_file(remote_path).await
    }
}

impl QueueRemoteFs {
    async fn check_file_size(
        remote_path: &str,
        expected_file_size: Option<u64>,
        local_path: &str,
        metadata: Metadata,
    ) -> Result<(), CubeError> {
        if let Some(expected_file_size) = expected_file_size {
            let actual_size = metadata.len();
            if actual_size != expected_file_size {
                tokio::fs::remove_file(local_path).await?;
                return Err(CubeError::corrupt_data(format!(
                    "Expected file size for '{}' is {} but {} received",
                    remote_path, expected_file_size, actual_size
                )));
            }
        }
        Ok(())
    }
}
