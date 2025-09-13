use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::Utc;
use hyper::{Body, Request as HttpReq, Response as HttpResp, Server as HttpServer};
use metrics::{counter, describe_counter, describe_histogram, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::Command,
    signal,
    sync::{broadcast, mpsc, RwLock},
};
use tokio_stream::wrappers::BroadcastStream;
use tonic::{transport::Server, Request, Response, Status};
use tonic_health::server::health_reporter;
use tracing::{error, info, warn};

pub mod pb { pub mod hub { pub mod v1 { include!("pb/hub.v1.rs"); } } }
use pb::hub::v1::hub_server::{Hub, HubServer};
use pb::hub::v1::*;

#[derive(Debug, Deserialize)]
struct HubConfig {
    #[serde(default = "default_addr")]
    addr: String,
    #[serde(default = "default_metrics_addr")]
    metrics_addr: String,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    adapters: Vec<AdapterCfg>,
}
fn default_addr() -> String { "0.0.0.0:50051".into() }
fn default_metrics_addr() -> String { "0.0.0.0:9090".into() }

#[derive(Debug, Deserialize)]
struct AdapterCfg {
    project_id: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Clone)]
struct ProjectCtx {
    events_tx: broadcast::Sender<Event>,
    to_adapter: mpsc::Sender<AdapterCmd>,
}

enum AdapterCmd { Run(RunRequest) }

#[derive(Default)]
struct Registry {
    projects: RwLock<HashMap<String, ProjectCtx>>,
}

#[derive(Clone)]
struct HubSvc {
    reg: Arc<Registry>,
    auth_token: Option<String>,
}

impl HubSvc {
    fn check_auth<T>(&self, req: &Request<T>) -> Result<(), Status> {
        if let Some(expected) = &self.auth_token {
            let md = req.metadata();
            match md.get("authorization").and_then(|v| v.to_str().ok()) {
                Some(val) if val == format!("Bearer {}", expected) => Ok(()),
                _ => Err(Status::unauthenticated("missing/invalid token")),
            }
        } else { Ok(()) }
    }
}

#[tonic::async_trait]
impl Hub for HubSvc {
    async fn run_on_project(
        &self,
        req: Request<RunRequest>,
    ) -> Result<Response<RunResponse>, Status> {
        self.check_auth(&req)?;
        let t0 = std::time::Instant::now();
        let r = req.into_inner();

        let Some(ctx) = self.reg.projects.read().await.get(&r.project_id).cloned() else {
            counter!("hub_requests_total", 1, "method" => "RunOnProject", "status" => "not_open");
            return Ok(Response::new(RunResponse { ok: false, error: "project not open".into() }));
        };

        match tokio::time::timeout(Duration::from_secs(5), ctx.to_adapter.send(AdapterCmd::Run(r))).await {
            Ok(Ok(_)) => {
                counter!("hub_requests_total", 1, "method" => "RunOnProject", "status" => "ok");
                histogram!("hub_request_latency_ms", t0.elapsed().as_millis() as f64, "method" => "RunOnProject");
                Ok(Response::new(RunResponse { ok: true, error: "".into() }))
            }
            _ => {
                counter!("hub_requests_total", 1, "method" => "RunOnProject", "status" => "busy");
                Ok(Response::new(RunResponse { ok: false, error: "adapter busy/offline".into() }))
            }
        }
    }

    type SubscribeStream = BroadcastStream<Event>;
    async fn subscribe(
        &self,
        req: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        self.check_auth(&req)?;
        let r = req.into_inner();
        let Some(ctx) = self.reg.projects.read().await.get(&r.project_id).cloned() else {
            return Err(Status::not_found("project not open"));
        };
        counter!("hub_requests_total", 1, "method" => "Subscribe", "status" => "ok");
        Ok(Response::new(BroadcastStream::new(ctx.events_tx.subscribe())))
    }

    async fn open_adapter(
        &self,
        req: Request<OpenAdapterRequest>,
    ) -> Result<Response<RunResponse>, Status> {
        self.check_auth(&req)?;
        let r = req.into_inner();
        if self.reg.projects.read().await.contains_key(&r.project_id) {
            return Ok(Response::new(RunResponse { ok: true, error: "".into() }));
        }
        open_adapter_impl(self.reg.clone(), &r.project_id, &r.command, &r.args).await
            .map_err(|e| Status::internal(e.to_string()))?;
        counter!("hub_requests_total", 1, "method" => "OpenAdapter", "status" => "ok");
        Ok(Response::new(RunResponse { ok: true, error: "".into() }))
    }

    async fn close_adapter(
        &self,
        req: Request<CloseAdapterRequest>,
    ) -> Result<Response<RunResponse>, Status> {
        self.check_auth(&req)?;
        let r = req.into_inner();
        self.reg.projects.write().await.remove(&r.project_id);
        counter!("hub_requests_total", 1, "method" => "CloseAdapter", "status" => "ok");
        Ok(Response::new(RunResponse { ok: true, error: "".into() }))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).compact().init();

    // metrics
    describe_counter!("hub_requests_total", "Total requests by method/status");
    describe_histogram!("hub_request_latency_ms", "Request latency in ms");
    let recorder = PrometheusBuilder::new().with_http_listener(([0,0,0,0], 9090)).install_recorder()?;
    drop(recorder); // hyper-less exporter runs its own http

    // config
    let cfg = if Path::new("hub.toml").exists() {
        toml::from_str::<HubConfig>(&std::fs::read_to_string("hub.toml")?)
            .context("parse hub.toml")?
    } else {
        HubConfig { addr: default_addr(), metrics_addr: default_metrics_addr(), auth_token: None, adapters: vec![] }
    };
    info!("config: addr={} metrics_addr={} auth={}",
          cfg.addr, cfg.metrics_addr, cfg.auth_token.as_deref().unwrap_or("<none>"));

    let addr = cfg.addr.parse().context("parse addr")?;

    // health + reflection
    let (mut health_reporter, health_service) = health_reporter();
    let reflection_service = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(tonic_reflection::pb::FILE_DESCRIPTOR_SET)
        .build()?;

    let reg = Arc::new(Registry::default());
    let hub = HubSvc { reg: reg.clone(), auth_token: cfg.auth_token.clone() };

    // auto-open adapters
    for a in cfg.adapters {
        if let Err(e) = open_adapter_impl(reg.clone(), &a.project_id, &a.command, &a.args).await {
            warn!("auto-open {} failed: {e}", a.project_id);
        } else {
            info!("auto-opened {}", a.project_id);
        }
    }

    health_reporter.set_serving::<HubServer<HubSvc>>().await;

    info!("gRPC hub listening on {}", addr);
    let grpc = Server::builder()
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .add_service(health_service)
        .add_service(reflection_service)
        .add_service(HubServer::new(hub))
        .serve(addr);

    tokio::select! {
        r = grpc => r?,
        _ = signal::ctrl_c() => { info!("shutdown signal"); }
    }
    Ok(())
}

// ---- Impl: ouverture adapter (process stdio) OU adapter intégré ----

async fn open_adapter_impl(reg: Arc<Registry>, project_id: &str, command: &str, args: &[String]) -> Result<()> {
    let (tx_cmd, mut rx_cmd) = mpsc::channel::<AdapterCmd>(256);
    let (ev_tx, _ev_rx) = broadcast::channel::<Event>(1024);

    if command == "builtin:echo" {
        // Adapter intégré : pas de process, juste une task
        let pid = project_id.to_string();
        let ev_tx2 = ev_tx.clone();
        tokio::spawn(async move {
            // simulateur de lecture événementielle
            while let Some(cmd) = rx_cmd.recv().await {
                match cmd {
                    AdapterCmd::Run(r) => {
                        let msg = serde_json::json!({
                            "level":"info",
                            "msg": format!("builtin echo run project={} max_passes={} mode={}", r.project_id, r.max_passes, r.mode),
                        });
                        let _ = ev_tx2.send(Event {
                            project_id: pid.clone(),
                            topic: "diagnostics".into(),
                            payload: Bytes::from(serde_json::to_vec(&msg).unwrap()).to_vec(),
                            ts_unix_ms: Utc::now().timestamp_millis(),
                        });
                    }
                }
            }
        });
        reg.projects.write().await.insert(project_id.to_string(), ProjectCtx { events_tx: ev_tx, to_adapter: tx_cmd });
        return Ok(());
    }

    // Sinon: process externe (stdio JSON lignes)
    let mut child = Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn adapter '{command}'"))?;

    let mut stdin = BufWriter::new(child.stdin.take().context("adapter stdin")?);
    let stdout = child.stdout.take().context("adapter stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    // writer: hub -> adapter
    tokio::spawn(async move {
        while let Some(cmd) = rx_cmd.recv().await {
            match cmd {
                AdapterCmd::Run(r) => {
                    let j = serde_json::json!({
                        "project_id": r.project_id,
                        "max_passes": r.max_passes,
                        "mode": r.mode,
                    });
                    let line = serde_json::to_string(&j).unwrap();
                    if stdin.write_all(line.as_bytes()).await.is_err() { break; }
                    let _ = stdin.write_all(b"\n").await;
                    let _ = stdin.flush().await;
                }
            }
        }
    });

    // reader: adapter -> events
    let pid = project_id.to_string();
    let ev_tx2 = ev_tx.clone();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let event = Event {
                project_id: pid.clone(),
                topic: "diagnostics".into(),
                payload: Bytes::from(line.into_bytes()).to_vec(),
                ts_unix_ms: Utc::now().timestamp_millis(),
            };
            let _ = ev_tx2.send(event);
        }
        let _ = ev_tx2.send(Event {
            project_id: pid.clone(),
            topic: "logs".into(),
            payload: b"{\"level\":\"warn\",\"msg\":\"adapter exited\"}".to_vec(),
            ts_unix_ms: Utc::now().timestamp_millis(),
        });
    });

    reg.projects.write().await.insert(project_id.to_string(), ProjectCtx { events_tx: ev_tx, to_adapter: tx_cmd });
    Ok(())
}
