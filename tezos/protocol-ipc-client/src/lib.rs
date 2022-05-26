// Copyright (c) SimpleStaking, Viable Systems and Tezedge Contributors
// SPDX-License-Identifier: MIT

//! This module implements a client that provides access to the protocol runners.
#![cfg_attr(feature = "fuzzing", feature(no_coverage))]

pub mod slog_level_serde;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_ipc::{IpcError, IpcReceiver, IpcSender};
use crypto::hash::{ChainId, ContextHash, ProtocolHash};
use serde::{Deserialize, Serialize};
use slog::{info, warn, Level, Logger};
use tezos_messages::p2p::encoding::operation::Operation;
use tezos_protocol_ipc_messages::*;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::Instant,
};

use tezos_api::{environment::TezosEnvironmentConfiguration, ffi::*};
use tezos_context_api::{
    ContextKeyOwned, ContextValue, PatchContext, StringTreeObject, TezosContextStorageConfiguration,
};

/// Errors generated by `protocol_runner`.
#[cfg_attr(feature = "fuzzing", derive(fuzzcheck::DefaultMutator))]
#[derive(Error, Serialize, Deserialize, Debug, Clone)]
pub enum ProtocolRunnerError {
    #[error("Failed to spawn tezos protocol wrapper sub-process: {reason}")]
    SpawnError { reason: String },
    #[error("Timeout when waiting for protocol runner connection socket")]
    SocketTimeout,
    #[error("Failed to terminate/kill tezos protocol wrapper sub-process, reason: {reason}")]
    TerminateError { reason: String },
}

impl From<tokio::io::Error> for ProtocolRunnerError {
    fn from(err: tokio::io::Error) -> Self {
        Self::SpawnError {
            reason: err.to_string(),
        }
    }
}

impl slog::Value for ProtocolRunnerError {
    fn serialize(
        &self,
        _record: &slog::Record,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_arguments(key, &format_args!("{}", self))
    }
}

/// Protocol configuration (transferred via IPC from tezedge node to protocol_runner.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProtocolRunnerConfiguration {
    pub runtime_configuration: TezosRuntimeConfiguration,
    pub environment: TezosEnvironmentConfiguration,
    pub enable_testchain: bool,
    pub storage: TezosContextStorageConfiguration,
    pub executable_path: PathBuf,
    #[serde(with = "slog_level_serde")]
    pub log_level: Level,
}

impl ProtocolRunnerConfiguration {
    pub fn new(
        runtime_configuration: TezosRuntimeConfiguration,
        environment: TezosEnvironmentConfiguration,
        enable_testchain: bool,
        storage: TezosContextStorageConfiguration,
        executable_path: PathBuf,
        log_level: Level,
    ) -> Self {
        Self {
            runtime_configuration,
            environment,
            enable_testchain,
            storage,
            executable_path,
            log_level,
        }
    }
}

// TODO: differentiate between writable and readonly runners?

struct IpcIO {
    rx: IpcReceiver<NodeMessage>,
    tx: IpcSender<ProtocolMessage>,
}

impl IpcIO {
    pub async fn send(&mut self, value: &ProtocolMessage) -> Result<(), async_ipc::IpcError> {
        self.tx.send(value).await?;
        Ok(())
    }

    pub async fn try_receive(
        &mut self,
        read_timeout: Option<Duration>,
    ) -> Result<NodeMessage, async_ipc::IpcError> {
        let result = if let Some(read_timeout) = read_timeout {
            self.rx.try_receive(read_timeout).await?
        } else {
            self.rx.receive().await?
        };
        Ok(result)
    }
}

/// Manages the execution of the protocol runners and access to their functionality.
#[derive(Clone)]
pub struct ProtocolRunnerApi {
    pub tokio_runtime: tokio::runtime::Handle,
    status_watcher: Arc<tokio::sync::Mutex<tokio::sync::watch::Receiver<bool>>>,
    log: Logger,
    socket_path: PathBuf,
    endpoint_name: String,
    configuration: ProtocolRunnerConfiguration,
}

impl ProtocolRunnerApi {
    pub fn new(
        configuration: ProtocolRunnerConfiguration,
        status_watcher: tokio::sync::watch::Receiver<bool>,
        tokio_runtime: &tokio::runtime::Handle,
        log: Logger,
    ) -> Self {
        Self {
            tokio_runtime: tokio_runtime.clone(),
            status_watcher: Arc::new(status_watcher.into()),
            log,
            socket_path: async_ipc::temp_sock(),
            endpoint_name: "writable-protocol-runner".to_owned(),
            configuration,
        }
    }

    /// Spawns protocol runners and returns once they start accepting connections.
    pub async fn start(&mut self, timeout: Option<Duration>) -> Result<Child, ProtocolRunnerError> {
        // TODO: what if wait_for_socket fails? child must be stopped
        let child = self.spawn()?;
        self.wait_for_socket(timeout).await?;

        Ok(child)
    }

    /// Spawns the protocol runner process if it is not running already
    fn spawn(&mut self) -> Result<Child, ProtocolRunnerError> {
        // Remove the socket file so that [`Self::wait_for_socket`] doesn't
        // prematurely find it before the protocol runner has started listening
        std::fs::remove_file(&self.socket_path).ok();

        let ProtocolRunnerConfiguration {
            executable_path,
            log_level,
            ..
        } = &self.configuration;
        let child = Self::spawn_process(
            executable_path,
            &self.socket_path,
            &self.endpoint_name,
            log_level,
            self.log.clone(),
            &self.tokio_runtime,
        )?;

        Ok(child)
    }

    /// Wait for socket to be ready (means that protocol-runner server started listening)
    async fn wait_for_socket(&self, timeout: Option<Duration>) -> Result<(), ProtocolRunnerError> {
        let start = Instant::now();
        let timeout = timeout.unwrap_or_else(|| Duration::from_secs(3));

        loop {
            if self.socket_path.exists() {
                break;
            }

            if start.elapsed() > timeout {
                return Err(ProtocolRunnerError::SocketTimeout);
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(())
    }

    fn spawn_process(
        executable_path: &Path,
        socket_path: &Path,
        endpoint_name: &str,
        log_level: &Level,
        log: Logger,
        tokio_runtime: &tokio::runtime::Handle,
    ) -> Result<tokio::process::Child, ProtocolRunnerError> {
        let _guard = tokio_runtime.enter();
        let mut process = Command::new(executable_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("--socket-path")
            .arg(socket_path)
            .arg("--endpoint")
            .arg(endpoint_name)
            .arg("--log-level")
            .arg(log_level.as_str().to_lowercase())
            .spawn()?;

        Self::log_subprocess_output(tokio_runtime, &mut process, log.clone());

        Ok(process)
    }

    /// Spawns a tokio task that will forward STDOUT and STDERR from the child
    /// process to slog's output
    fn log_subprocess_output(
        tokio_runtime: &tokio::runtime::Handle,
        process: &mut Child,
        log: Logger,
    ) {
        // Only launch logging task if the output port if present, otherwise log a warning.
        macro_rules! handle_output {
            ($tag:expr, $name:expr, $io:expr, $log:expr) => {{
                if let Some(out) = $io.take() {
                    let log = $log;
                    tokio_runtime.spawn(async move {
                        let reader = BufReader::new(out);
                        let mut lines = reader.lines();
                        loop {
                            match lines.next_line().await {
                                Ok(Some(line)) => info!(log, "[{}] {}", $tag, line),
                                Ok(None) => {
                                    info!(log, "[{}] {} closed.", $tag, $name);
                                    break;
                                }
                                Err(err) => {
                                    warn!(log, "[{}] {} closed with error: {:?}", $tag, $name, err);
                                    break;
                                }
                            }
                        }
                    });
                } else {
                    warn!(
                        log,
                        "Expected child process to have {}, but it was None", $name
                    );
                };
            }};
        }

        handle_output!("OCaml-out", "STDOUT", process.stdout, log.clone());
        handle_output!("OCaml-err", "STDERR", process.stderr, log.clone());
    }

    pub async fn wait_for_context_init(&self) -> Result<(), tokio::sync::watch::error::RecvError> {
        let mut watcher = self.status_watcher.lock().await;
        loop {
            if *watcher.borrow_and_update() {
                break;
            }
            watcher.changed().await?;
        }
        Ok(())
    }

    pub fn wait_for_context_init_sync(&self) -> Result<(), tokio::sync::watch::error::RecvError> {
        tokio::task::block_in_place(|| self.tokio_runtime.block_on(self.wait_for_context_init()))
    }

    /// Connect to protocol runner without waiting for context initialization.
    pub async fn connect(&self) -> Result<ProtocolRunnerConnection, IpcError> {
        let ipc_client = async_ipc::IpcClient::new(&self.socket_path);
        let (rx, tx) = ipc_client.connect().await?;
        let io = IpcIO { rx, tx };

        Ok(ProtocolRunnerConnection {
            configuration: self.configuration.clone(),
            io,
        })
    }

    /// Obtains a connection to a protocol runner instance with read access to the context.
    ///
    /// Waits for protocol runner to be running and context to be initialized.
    pub async fn readable_connection(&self) -> Result<ProtocolRunnerConnection, IpcError> {
        let _ = self.wait_for_context_init().await;
        self.connect().await
    }

    /// Like [`Self::readable_connection`] but callable from non-async functions.
    pub fn readable_connection_sync(&self) -> Result<ProtocolRunnerConnection, IpcError> {
        tokio::task::block_in_place(|| self.tokio_runtime.block_on(self.readable_connection()))
    }
}

pub struct ProtocolRunnerConnection {
    pub configuration: ProtocolRunnerConfiguration,
    io: IpcIO,
}

macro_rules! handle_request {
    ($io:expr, $msg:ident $(($($arg:ident),+))?, $resp:ident($result:ident), $error:ident, $timeout:expr $(,)?) => {{
        let msg = ProtocolMessage::$msg $(($($arg),+))?;
        $io.send(&msg).await?;

        match $io.try_receive($timeout).await? {
            NodeMessage::$resp($result) => {
                $result.map_err(|err| ProtocolError::$error { reason: err }.into())
            }
            message => Err(ProtocolServiceError::UnexpectedMessage {
                message: message.into(),
            }),
        }
    }};

    ($io:expr, $msg:ident $(($($arg:ident),+))?, $resp:ident $(($result:ident))? => $result_expr:expr, $timeout:expr $(,)?) => {{
        $io.send(&ProtocolMessage::$msg $(($($arg),+))?).await?;

        match $io.try_receive($timeout).await? {
            NodeMessage::$resp $(($result))? => $result_expr,
            message => Err(ProtocolServiceError::UnexpectedMessage {
                message: message.into(),
            }),
        }
    }};
}

impl ProtocolRunnerConnection {
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
    const DEFAULT_TIMEOUT_LONG: Duration = Duration::from_secs(60 * 2);
    const DEFAULT_TIMEOUT_VERY_LONG: Duration = Duration::from_secs(60 * 30);

    const APPLY_BLOCK_TIMEOUT: Duration = Duration::from_secs(60 * 240);
    const GET_LATEST_CONTEXT_HASHES_TIMEOUT: Duration = Self::APPLY_BLOCK_TIMEOUT; // Reloading the context from disk might takes a long time
    const INIT_PROTOCOL_CONTEXT_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const BEGIN_APPLICATION_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const BEGIN_CONSTRUCTION_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const VALIDATE_OPERATION_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const CALL_PROTOCOL_RPC_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const CALL_PROTOCOL_HEAVY_RPC_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_VERY_LONG;
    const COMPUTE_PATH_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const JSON_ENCODE_DATA_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const ASSERT_ENCODING_FOR_PROTOCOL_DATA_TIMEOUT: Duration = Self::DEFAULT_TIMEOUT_LONG;
    const PING_TIMEOUT: Duration = Duration::from_secs(1);

    /// Apply block
    pub async fn apply_block(
        &mut self,
        request: ApplyBlockRequest,
    ) -> Result<ApplyBlockResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            ApplyBlockCall(request),
            ApplyBlockResult(result),
            ApplyBlockError,
            Some(Self::APPLY_BLOCK_TIMEOUT),
        )
    }

    /// Get latest context hashes
    pub async fn latest_context_hashes(
        &mut self,
        count: i64,
    ) -> Result<Vec<ContextHash>, ProtocolServiceError> {
        handle_request!(
            self.io,
            ContextGetLatestContextHashes(count),
            ContextGetLatestContextHashesResult(result),
            GetLastContextHashesError,
            Some(Self::GET_LATEST_CONTEXT_HASHES_TIMEOUT),
        )
    }

    pub async fn assert_encoding_for_protocol_data(
        &mut self,
        protocol_hash: ProtocolHash,
        protocol_data: RustBytes,
    ) -> Result<(), ProtocolServiceError> {
        handle_request!(
            self.io,
            AssertEncodingForProtocolDataCall(protocol_hash, protocol_data),
            AssertEncodingForProtocolDataResult(result),
            AssertEncodingForProtocolDataError,
            Some(Self::ASSERT_ENCODING_FOR_PROTOCOL_DATA_TIMEOUT),
        )
    }

    /// Begin application
    pub async fn begin_application(
        &mut self,
        request: BeginApplicationRequest,
    ) -> Result<BeginApplicationResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            BeginApplicationCall(request),
            BeginApplicationResult(result),
            BeginApplicationError,
            Some(Self::BEGIN_APPLICATION_TIMEOUT),
        )
    }

    /// Begin construction
    pub async fn begin_construction(
        &mut self,
        request: BeginConstructionRequest,
    ) -> Result<PrevalidatorWrapper, ProtocolServiceError> {
        handle_request!(
            self.io,
            BeginConstruction(request),
            BeginConstructionResult(result),
            BeginConstructionError,
            Some(Self::BEGIN_CONSTRUCTION_TIMEOUT),
        )
    }

    /// Pre-filter operation
    pub async fn pre_filter_operation(
        &mut self,
        request: ValidateOperationRequest,
    ) -> Result<PreFilterOperationResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            PreFilterOperation(request),
            PreFilterOperationResult(result),
            PreFilterOperationError,
            Some(Self::VALIDATE_OPERATION_TIMEOUT),
        )
    }

    /// Validate operation
    pub async fn validate_operation(
        &mut self,
        request: ValidateOperationRequest,
    ) -> Result<ValidateOperationResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            ValidateOperation(request),
            ValidateOperationResponse(result),
            ValidateOperationError,
            Some(Self::VALIDATE_OPERATION_TIMEOUT),
        )
    }

    /// ComputePath
    pub async fn compute_path(
        &mut self,
        request: ComputePathRequest,
    ) -> Result<ComputePathResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            ComputePathCall(request),
            ComputePathResponse(result),
            ComputePathError,
            Some(Self::COMPUTE_PATH_TIMEOUT),
        )
    }

    pub async fn apply_block_result_metadata(
        &mut self,
        context_hash: ContextHash,
        metadata_bytes: RustBytes,
        max_operations_ttl: i32,
        protocol_hash: ProtocolHash,
        next_protocol_hash: ProtocolHash,
    ) -> Result<String, ProtocolServiceError> {
        let params = JsonEncodeApplyBlockResultMetadataParams {
            context_hash,
            max_operations_ttl,
            metadata_bytes,
            protocol_hash,
            next_protocol_hash,
        };

        handle_request!(
            self.io,
            JsonEncodeApplyBlockResultMetadata(params),
            JsonEncodeApplyBlockResultMetadataResponse(result) => result.map_err(|err| {
                ProtocolError::FfiJsonEncoderError {
                    caller: "apply_block_result_metadata".to_owned(),
                    reason: err,
                }
                .into()
            }),
            Some(Self::JSON_ENCODE_DATA_TIMEOUT),
        )
    }

    pub async fn apply_block_operations_metadata(
        &mut self,
        chain_id: ChainId,
        operations: Vec<Vec<Operation>>,
        operations_metadata_bytes: Vec<Vec<RustBytes>>,
        protocol_hash: ProtocolHash,
        next_protocol_hash: ProtocolHash,
    ) -> Result<String, ProtocolServiceError> {
        let params = JsonEncodeApplyBlockOperationsMetadataParams {
            chain_id,
            operations,
            operations_metadata_bytes,
            protocol_hash,
            next_protocol_hash,
        };

        handle_request!(
            self.io,
            JsonEncodeApplyBlockOperationsMetadata(params),
            JsonEncodeApplyBlockOperationsMetadata(result) => result.map_err(|err| {
                ProtocolError::FfiJsonEncoderError {
                    caller: "apply_block_operations_metadata".to_owned(),
                    reason: err,
                }
                .into()
            }),
            Some(Self::JSON_ENCODE_DATA_TIMEOUT),
        )
    }

    /// Call protocol  rpc - internal
    async fn call_protocol_rpc_internal(
        &mut self,
        request_path: String,
        request: ProtocolRpcRequest,
    ) -> Result<ProtocolRpcResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            ProtocolRpcCall(request),
            RpcResponse(result) => result.map_err(|err| {
                ProtocolError::ProtocolRpcError {
                    reason: err,
                    request_path,
                }
                .into()
            }),
            Some(Self::CALL_PROTOCOL_HEAVY_RPC_TIMEOUT),
        )
    }

    /// Call protocol rpc
    pub async fn call_protocol_rpc(
        &mut self,
        request: ProtocolRpcRequest,
    ) -> Result<ProtocolRpcResponse, ProtocolServiceError> {
        self.call_protocol_rpc_internal(request.request.context_path.clone(), request)
            .await
    }

    /// Call helpers_preapply_operations shell service
    pub async fn helpers_preapply_operations(
        &mut self,
        request: ProtocolRpcRequest,
    ) -> Result<HelpersPreapplyResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            HelpersPreapplyOperationsCall(request),
            HelpersPreapplyResponse(result),
            HelpersPreapplyError,
            Some(Self::CALL_PROTOCOL_RPC_TIMEOUT),
        )
    }

    /// Call helpers_preapply_block shell service
    pub async fn helpers_preapply_block(
        &mut self,
        request: HelpersPreapplyBlockRequest,
    ) -> Result<HelpersPreapplyResponse, ProtocolServiceError> {
        handle_request!(
            self.io,
            HelpersPreapplyBlockCall(request),
            HelpersPreapplyResponse(result),
            HelpersPreapplyError,
            Some(Self::CALL_PROTOCOL_RPC_TIMEOUT),
        )
    }

    /// Change tezos runtime configuration
    pub async fn change_runtime_configuration(
        &mut self,
        settings: TezosRuntimeConfiguration,
    ) -> Result<(), ProtocolServiceError> {
        handle_request!(
            self.io,
            ChangeRuntimeConfigurationCall(settings),
            ChangeRuntimeConfigurationResult => Ok(()),
            Some(Self::DEFAULT_TIMEOUT),
        )
    }

    /// Command tezos ocaml code to initialize context and protocol.
    /// CommitGenesisResult is returned only if commit_genesis is set to true
    #[allow(clippy::too_many_arguments)]
    async fn init_protocol_context(
        &mut self,
        storage: TezosContextStorageConfiguration,
        tezos_environment: &TezosEnvironmentConfiguration,
        commit_genesis: bool,
        enable_testchain: bool,
        readonly: bool,
        patch_context: Option<PatchContext>,
        context_stats_db_path: Option<PathBuf>,
    ) -> Result<InitProtocolContextResult, ProtocolServiceError> {
        let params = InitProtocolContextParams {
            storage,
            genesis: tezos_environment.genesis.clone(),
            genesis_max_operations_ttl: tezos_environment
                .genesis_additional_data()
                .map_err(|error| ProtocolServiceError::InvalidDataError {
                    message: format!("{:?}", error),
                })?
                .max_operations_ttl,
            protocol_overrides: tezos_environment.protocol_overrides.clone(),
            commit_genesis,
            enable_testchain,
            readonly,
            patch_context,
            context_stats_db_path,
        };

        self.init_protocol_context_raw(params).await
    }

    pub async fn init_protocol_context_raw(
        &mut self,
        params: InitProtocolContextParams,
    ) -> Result<InitProtocolContextResult, ProtocolServiceError> {
        handle_request!(
            self.io,
            InitProtocolContextCall(params),
            InitProtocolContextResult(result),
            OcamlStorageInitError,
            Some(Self::INIT_PROTOCOL_CONTEXT_TIMEOUT),
        )
    }

    /// Ping the protocol runner
    pub async fn ping(&mut self) -> Result<(), ProtocolServiceError> {
        handle_request!(
            self.io,
            Ping,
            PingResult => Ok(()),
            Some(Self::PING_TIMEOUT),
        )
    }

    /// Gracefully shutdown protocol runner
    pub async fn shutdown(&mut self) -> Result<(), ProtocolServiceError> {
        handle_request!(
            self.io,
            ShutdownCall,
            ShutdownResult => Ok(()),
            Some(Self::DEFAULT_TIMEOUT),
        )
    }

    /// Initialize protocol environment from default configuration (writeable).
    pub async fn init_protocol_for_write(
        &mut self,
        commit_genesis: bool,
        patch_context: &Option<PatchContext>,
        context_stats_db_path: Option<PathBuf>,
    ) -> Result<InitProtocolContextResult, ProtocolServiceError> {
        self.change_runtime_configuration(self.configuration.runtime_configuration.clone())
            .await?;
        let environment = self.configuration.environment.clone();
        let result = self
            .init_protocol_context(
                self.configuration.storage.clone(),
                &environment,
                commit_genesis,
                self.configuration.enable_testchain,
                false,
                patch_context.clone(),
                context_stats_db_path,
            )
            .await?;

        // Initialize the contexct IPC server to serve reads from readonly protocol runners
        self.init_context_ipc_server().await?;

        Ok(result)
    }

    /// Initialize protocol environment from default configuration (readonly).
    pub async fn init_protocol_for_read(
        &mut self,
    ) -> Result<InitProtocolContextResult, ProtocolServiceError> {
        // TODO - TE-261: should use a different message exchange for readonly contexts?
        self.change_runtime_configuration(self.configuration.runtime_configuration.clone())
            .await?;
        let environment = self.configuration.environment.clone();
        self.init_protocol_context(
            self.configuration.storage.clone(),
            &environment,
            false,
            self.configuration.enable_testchain,
            true,
            None,
            None,
        )
        .await
    }

    // TODO - TE-261: this requires more descriptive errors.

    /// Initializes server to listen for readonly context clients through IPC.
    ///
    /// Must be called after the writable context has been initialized.
    pub async fn init_context_ipc_server(&mut self) -> Result<(), ProtocolServiceError> {
        if self.configuration.storage.get_ipc_socket_path().is_some() {
            self.init_context_ipc_server_raw(self.configuration.storage.clone())
                .await
        } else {
            Ok(())
        }
    }

    pub async fn init_context_ipc_server_raw(
        &mut self,
        cfg: TezosContextStorageConfiguration,
    ) -> Result<(), ProtocolServiceError> {
        handle_request!(
            self.io,
            InitProtocolContextIpcServer(cfg),
            InitProtocolContextIpcServerResult(result) => {
                result.map_err(|err| ProtocolServiceError::ContextIpcServerError {
                    message: format!("Failure when starting context IPC server: {}", err),
                })
            },
            Some(Self::DEFAULT_TIMEOUT),
        )
    }

    /// Gets data for genesis.
    pub async fn genesis_result_data(
        &mut self,
        genesis_context_hash: &ContextHash,
    ) -> Result<CommitGenesisResult, ProtocolServiceError> {
        let tezos_environment = self.configuration.environment.clone();
        let main_chain_id = tezos_environment.main_chain_id().map_err(|e| {
            ProtocolServiceError::InvalidDataError {
                message: format!("{:?}", e),
            }
        })?;
        let protocol_hash = tezos_environment.genesis_protocol().map_err(|e| {
            ProtocolServiceError::InvalidDataError {
                message: format!("{:?}", e),
            }
        })?;

        self.genesis_result_data_raw(GenesisResultDataParams {
            genesis_context_hash: genesis_context_hash.clone(),
            chain_id: main_chain_id,
            genesis_protocol_hash: protocol_hash,
            genesis_max_operations_ttl: tezos_environment
                .genesis_additional_data()
                .map_err(|error| ProtocolServiceError::InvalidDataError {
                    message: format!("{:?}", error),
                })?
                .max_operations_ttl,
        })
        .await
    }

    pub async fn genesis_result_data_raw(
        &mut self,
        params: GenesisResultDataParams,
    ) -> Result<CommitGenesisResult, ProtocolServiceError> {
        handle_request!(
            self.io,
            GenesisResultDataCall(params),
            CommitGenesisResultData(result),
            GenesisResultDataError,
            Some(Self::DEFAULT_TIMEOUT),
        )
    }

    pub async fn get_context_key_from_history(
        &mut self,
        context_hash: &ContextHash,
        key: ContextKeyOwned,
    ) -> Result<Option<ContextValue>, ProtocolServiceError> {
        let params = ContextGetKeyFromHistoryRequest {
            context_hash: context_hash.clone(),
            key,
        };

        handle_request!(
            self.io,
            ContextGetKeyFromHistory(params),
            ContextGetKeyFromHistoryResult(result),
            ContextGetKeyFromHistoryError,
            Some(Self::DEFAULT_TIMEOUT),
        )
    }

    pub async fn get_context_key_values_by_prefix(
        &mut self,
        context_hash: &ContextHash,
        prefix: ContextKeyOwned,
    ) -> Result<Option<Vec<(ContextKeyOwned, ContextValue)>>, ProtocolServiceError> {
        let params = ContextGetKeyValuesByPrefixRequest {
            context_hash: context_hash.clone(),
            prefix,
        };

        handle_request!(
            self.io,
            ContextGetKeyValuesByPrefix(params),
            ContextGetKeyValuesByPrefixResult(result),
            ContextGetKeyValuesByPrefixError,
            Some(Self::DEFAULT_TIMEOUT_VERY_LONG),
        )
    }

    pub async fn get_context_tree_by_prefix(
        &mut self,
        context_hash: &ContextHash,
        prefix: ContextKeyOwned,
        depth: Option<usize>,
    ) -> Result<StringTreeObject, ProtocolServiceError> {
        let params = ContextGetTreeByPrefixRequest {
            context_hash: context_hash.clone(),
            prefix,
            depth,
        };

        handle_request!(
            self.io,
            ContextGetTreeByPrefix(params),
            ContextGetTreeByPrefixResult(result),
            ContextGetKeyValuesByPrefixError,
            Some(Self::DEFAULT_TIMEOUT_VERY_LONG),
        )
    }

    pub async fn dump_context(
        &mut self,
        context_hash: ContextHash,
        dump_into_path: String,
    ) -> Result<i64, ProtocolServiceError> {
        let request = DumpContextRequest {
            context_hash,
            dump_into_path,
        };

        handle_request!(
            self.io,
            DumpContext(request),
            DumpContextResponse(result),
            DumpContextError,
            None,
        )
    }

    pub async fn restore_context(
        &mut self,
        expected_context_hash: ContextHash,
        restore_from_path: String,
        nb_context_elements: i64,
    ) -> Result<(), ProtocolServiceError> {
        let request = RestoreContextRequest {
            expected_context_hash,
            restore_from_path,
            nb_context_elements,
        };

        handle_request!(
            self.io,
            RestoreContext(request),
            RestoreContextResponse(result),
            RestoreContextError,
            None,
        )
    }
}

// Errors

/// Errors generated by `protocol_runner`.
#[cfg_attr(feature = "fuzzing", derive(fuzzcheck::DefaultMutator))]
#[derive(Error, Serialize, Deserialize, Debug, Clone)]
pub enum ProtocolServiceError {
    /// Generic IPC communication error. See `reason` for more details.
    #[error("IPC error: {reason}")]
    IpcError {
        #[from]
        reason: IpcError,
    },
    /// Tezos protocol error.
    #[error("Protocol error: {reason}")]
    ProtocolError {
        #[from]
        reason: ProtocolError,
    },
    /// Unexpected message was received from IPC channel
    #[error("Received unexpected message: {message:?}")]
    UnexpectedMessage { message: NodeMessageKind },
    /// Invalid data error
    #[error("Invalid data error: {message}")]
    InvalidDataError { message: String },
    /// Lock error
    #[error("Lock error: {message:?}")]
    LockPoisonError { message: String },
    /// Context IPC server error
    #[error("Context IPC server error: {message:?}")]
    ContextIpcServerError { message: String },
}

impl ProtocolServiceError {
    /// Returns true if this a context hash mismatch error for which the cache was loaded
    pub fn is_cache_context_hash_mismatch_error(&self) -> bool {
        if let Self::ProtocolError { reason } = self {
            reason.is_cache_context_hash_mismatch_error()
        } else {
            false
        }
    }
}

impl<T> From<std::sync::PoisonError<T>> for ProtocolServiceError {
    fn from(source: std::sync::PoisonError<T>) -> Self {
        Self::LockPoisonError {
            message: source.to_string(),
        }
    }
}

impl slog::Value for ProtocolServiceError {
    fn serialize(
        &self,
        _record: &slog::Record,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_arguments(key, &format_args!("{}", self))
    }
}

pub fn handle_protocol_service_error<LC: Fn(ProtocolServiceError)>(
    error: ProtocolServiceError,
    log_callback: LC,
) -> Result<(), ProtocolServiceError> {
    match error {
        ProtocolServiceError::IpcError { .. } | ProtocolServiceError::UnexpectedMessage { .. } => {
            // we need to refresh protocol runner endpoint, so propagate error
            Err(error)
        }
        _ => {
            // just log error
            log_callback(error);
            Ok(())
        }
    }
}
