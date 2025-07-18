// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;

use anyhow::Context;
use async_trait::async_trait;
use mz_authenticator::Authenticator;
use mz_ore::now::{SYSTEM_TIME, epoch_to_uuid_v7};
use mz_pgwire_common::{
    ACCEPT_SSL_ENCRYPTION, CONN_UUID_KEY, Conn, ConnectionCounter, FrontendStartupMessage,
    MZ_FORWARDED_FOR_KEY, REJECT_ENCRYPTION, decode_startup,
};
use mz_server_core::listeners::AllowedRoles;
use mz_server_core::{Connection, ConnectionHandler, ReloadingTlsConfig};
use openssl::ssl::Ssl;
use tokio::io::AsyncWriteExt;
use tokio_metrics::TaskMetrics;
use tokio_openssl::SslStream;
use tracing::{debug, error, trace};

use crate::codec::FramedConn;
use crate::metrics::{Metrics, MetricsConfig};
use crate::protocol;

/// Configures a [`Server`].
#[derive(Debug)]
pub struct Config {
    /// The label for the mz_connection_status metric.
    pub label: &'static str,
    /// A client for the adapter with which the server will communicate.
    pub adapter_client: mz_adapter::Client,
    /// The TLS configuration for the server.
    ///
    /// If not present, then TLS is not enabled, and clients requests to
    /// negotiate TLS will be rejected.
    pub tls: Option<ReloadingTlsConfig>,
    /// Authentication method to use. Frontegg, Password, or None.
    pub authenticator: Authenticator,
    /// The registry entries that the pgwire server uses to report metrics.
    pub metrics: MetricsConfig,
    /// Global connection limit and count
    pub active_connection_counter: ConnectionCounter,
    /// Helm chart version
    pub helm_chart_version: Option<String>,
    /// Whether to allow reserved users (ie: mz_system).
    pub allowed_roles: AllowedRoles,
}

/// A server that communicates with clients via the pgwire protocol.
pub struct Server {
    tls: Option<ReloadingTlsConfig>,
    adapter_client: mz_adapter::Client,
    authenticator: Authenticator,
    metrics: Metrics,
    active_connection_counter: ConnectionCounter,
    helm_chart_version: Option<String>,
    allowed_roles: AllowedRoles,
}

#[async_trait]
impl mz_server_core::Server for Server {
    const NAME: &'static str = "pgwire";

    fn handle_connection(
        &self,
        conn: Connection,
        tokio_metrics_intervals: impl Iterator<Item = TaskMetrics> + Send + 'static,
    ) -> ConnectionHandler {
        // Using fully-qualified syntax means we won't accidentally call
        // ourselves (i.e., silently infinitely recurse) if the name or type of
        // `crate::Server::handle_connection` changes.
        Box::pin(crate::Server::handle_connection(
            self,
            conn,
            tokio_metrics_intervals,
        ))
    }
}

impl Server {
    /// Constructs a new server.
    pub fn new(config: Config) -> Server {
        Server {
            tls: config.tls,
            adapter_client: config.adapter_client,
            authenticator: config.authenticator,
            metrics: Metrics::new(config.metrics, config.label),
            active_connection_counter: config.active_connection_counter,
            helm_chart_version: config.helm_chart_version,
            allowed_roles: config.allowed_roles,
        }
    }

    #[mz_ore::instrument(level = "debug")]
    pub fn handle_connection(
        &self,
        conn: Connection,
        tokio_metrics_intervals: impl Iterator<Item = TaskMetrics> + Send + 'static,
    ) -> impl Future<Output = Result<(), anyhow::Error>> + Send + 'static {
        let adapter_client = self.adapter_client.clone();
        let authenticator = self.authenticator.clone();
        let tls = self.tls.clone();
        let metrics = self.metrics.clone();
        let active_connection_counter = self.active_connection_counter.clone();
        let helm_chart_version = self.helm_chart_version.clone();
        let allowed_roles = self.allowed_roles;

        // TODO(guswynn): remove this redundant_closure_call
        #[allow(clippy::redundant_closure_call)]
        async move {
            let result = (|| {
                async move {
                    let conn_id = adapter_client.new_conn_id()?;
                    let mut conn = Conn::Unencrypted(conn);
                    loop {
                        let message = decode_startup(&mut conn).await?;

                        match &message {
                            Some(message) => trace!("cid={} recv={:?}", conn_id, message),
                            None => trace!("cid={} recv=<eof>", conn_id),
                        }

                        conn = match message {
                            // Clients sometimes hang up during the startup sequence, e.g.
                            // because they receive an unacceptable response to an
                            // `SslRequest`. This is considered a graceful termination.
                            None => return Ok(()),

                            Some(FrontendStartupMessage::Startup {
                                version,
                                mut params,
                            }) => {
                                // If someone (usually the balancer) forwarded a connection UUID,
                                // then use that, otherwise generate one.
                                let conn_uuid_handle = conn.inner_mut().uuid_handle();
                                let conn_uuid = params
                                    .remove(CONN_UUID_KEY)
                                    .and_then(|uuid| uuid.parse().inspect_err(|e| error!("pgwire connection with invalid conn UUID: {e}")).ok());
                                let conn_uuid_forwarded = conn_uuid.is_some();
                                // FIXME(ptravers): we should be able to inject the clock when instantiating the `Server`
                                // but as of writing there's no great way, I can see, to harmonize the lifetimes of the return type
                                // and &self which must house `NowFn`.
                                let conn_uuid = conn_uuid.unwrap_or_else(|| epoch_to_uuid_v7(&(SYSTEM_TIME.clone())()));
                                conn_uuid_handle.set(conn_uuid);
                                debug!(conn_uuid = %conn_uuid_handle.display(), conn_uuid_forwarded, "starting new pgwire connection in adapter");

                                let direct_peer_addr = conn
                                    .inner_mut()
                                    .peer_addr()
                                    .context("fetching peer addr")?
                                    .ip();
                                let peer_addr= match params.remove(MZ_FORWARDED_FOR_KEY) {
                                    Some(ip_str) => {
                                        match IpAddr::from_str(&ip_str) {
                                            Ok(ip) => Some(ip),
                                            Err(e) => {
                                                error!("pgwire connection with invalid mz_forwarded_for address: {e}");
                                                None
                                            }
                                        }
                                    }
                                    None => Some(direct_peer_addr)
                                };
                                let mut conn = FramedConn::new(
                                    conn_id.clone(),
                                    peer_addr,
                                    conn,
                                );

                                protocol::run(protocol::RunParams {
                                    tls_mode: tls.as_ref().map(|tls| tls.mode),
                                    adapter_client,
                                    conn: &mut conn,
                                    conn_uuid,
                                    version,
                                    params,
                                    authenticator,
                                    active_connection_counter,
                                    helm_chart_version,
                                    allowed_roles,
                                    tokio_metrics_intervals,
                                })
                                .await?;
                                conn.flush().await?;
                                return Ok(());
                            }

                            Some(FrontendStartupMessage::CancelRequest {
                                conn_id,
                                secret_key,
                            }) => {
                                adapter_client.cancel_request(conn_id, secret_key);
                                // For security, the client is not told whether the cancel
                                // request succeeds or fails.
                                return Ok(());
                            }

                            Some(FrontendStartupMessage::SslRequest) => match (conn, &tls) {
                                (Conn::Unencrypted(mut conn), Some(tls)) => {
                                    trace!("cid={} send=AcceptSsl", conn_id);
                                    conn.write_all(&[ACCEPT_SSL_ENCRYPTION]).await?;
                                    let mut ssl_stream =
                                        SslStream::new(Ssl::new(&tls.context.get())?, conn)?;
                                    if let Err(e) = Pin::new(&mut ssl_stream).accept().await {
                                        let _ = ssl_stream.get_mut().shutdown().await;
                                        return Err(e.into());
                                    }
                                    Conn::Ssl(ssl_stream)
                                }
                                (mut conn, _) => {
                                    trace!("cid={} send=RejectSsl", conn_id);
                                    conn.write_all(&[REJECT_ENCRYPTION]).await?;
                                    conn
                                }
                            },

                            Some(FrontendStartupMessage::GssEncRequest) => {
                                trace!("cid={} send=RejectGssEnc", conn_id);
                                conn.write_all(&[REJECT_ENCRYPTION]).await?;
                                conn
                            }
                        }
                    }
                }
            })()
            .await;
            metrics.connection_status(result.is_ok()).inc();
            result
        }
    }
}
