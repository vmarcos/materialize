// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Integration tests for balancerd.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use jsonwebtoken::{DecodingKey, EncodingKey};
use mz_balancerd::{BalancerConfig, BalancerService, FronteggResolver, Resolver, BUILD_INFO};
use mz_environmentd::test_util::{self, make_pg_tls, Ca};
use mz_frontegg_auth::{
    Authentication as FronteggAuthentication, AuthenticationConfig as FronteggConfig,
};
use mz_frontegg_mock::FronteggMockServer;
use mz_ore::cast::CastFrom;
use mz_ore::metrics::MetricsRegistry;
use mz_ore::now::SYSTEM_TIME;
use mz_ore::retry::Retry;
use mz_ore::{assert_contains, task};
use mz_server_core::TlsCertConfig;
use openssl::ssl::{SslConnectorBuilder, SslVerifyMode};
use uuid::Uuid;

#[mz_ore::test(tokio::test(flavor = "multi_thread", worker_threads = 1))]
#[cfg_attr(miri, ignore)] // too slow
async fn test_balancer() {
    let ca = Ca::new_root("test ca").unwrap();
    let (server_cert, server_key) = ca
        .request_cert("server", vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .unwrap();
    let metrics_registry = MetricsRegistry::new();

    let tenant_id = Uuid::new_v4();
    let client_id = Uuid::new_v4();
    let secret = Uuid::new_v4();
    let users = BTreeMap::from([(
        (client_id.to_string(), secret.to_string()),
        "user@_.com".to_string(),
    )]);
    let roles = BTreeMap::from([("user@_.com".to_string(), Vec::new())]);
    let encoding_key =
        EncodingKey::from_rsa_pem(&ca.pkey.private_key_to_pem_pkcs8().unwrap()).unwrap();

    const EXPIRES_IN_SECS: i64 = 50;
    let frontegg_server = FronteggMockServer::start(
        None,
        encoding_key,
        tenant_id,
        users,
        roles,
        SYSTEM_TIME.clone(),
        EXPIRES_IN_SECS,
        // Add a bit of delay so we can test connection de-duplication.
        Some(Duration::from_millis(100)),
    )
    .unwrap();

    let frontegg_auth = FronteggAuthentication::new(
        FronteggConfig {
            admin_api_token_url: frontegg_server.url.clone(),
            decoding_key: DecodingKey::from_rsa_pem(&ca.pkey.public_key_to_pem().unwrap()).unwrap(),
            tenant_id: Some(tenant_id),
            now: SYSTEM_TIME.clone(),
            admin_role: "mzadmin".to_string(),
        },
        mz_frontegg_auth::Client::default(),
        &metrics_registry,
    );
    let frontegg_user = "user@_.com";
    let frontegg_password = format!("mzp_{client_id}{secret}");

    let envd_server = test_util::TestHarness::default()
        // Enable SSL on the main port. There should be a balancerd port with no SSL.
        .with_tls(server_cert.clone(), server_key.clone())
        .with_frontegg(&frontegg_auth)
        .with_metrics_registry(metrics_registry)
        .start()
        .await;

    // Ensure we could connect directly to envd without SSL on the balancer port.
    let pg_client_envd = envd_server
        .connect()
        .balancer()
        .user(frontegg_user)
        .password(&frontegg_password)
        .await
        .unwrap();

    let res: i32 = pg_client_envd
        .query_one("SELECT 4", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(res, 4);

    let resolvers = vec![
        Resolver::Static(envd_server.inner.balancer_sql_local_addr().to_string()),
        Resolver::Frontegg(FronteggResolver {
            auth: frontegg_auth,
            addr_template: envd_server.inner.balancer_sql_local_addr().to_string(),
        }),
    ];
    let cert_config = Some(TlsCertConfig {
        cert: server_cert,
        key: server_key,
    });

    for resolver in resolvers {
        let is_frontegg_resolver = matches!(resolver, Resolver::Frontegg(_));
        let balancer_cfg = BalancerConfig::new(
            &BUILD_INFO,
            None,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            Some(envd_server.inner.balancer_sql_local_addr().to_string()),
            resolver,
            envd_server.inner.http_local_addr().to_string(),
            cert_config.clone(),
            MetricsRegistry::new(),
        );
        let balancer_server = BalancerService::new(balancer_cfg).await.unwrap();
        let balancer_pgwire_listen = balancer_server.pgwire.0.local_addr();
        task::spawn(|| "balancer", async {
            balancer_server.serve().await.unwrap();
        });

        let conn_str = Arc::new(format!(
            "user={frontegg_user} password={frontegg_password} host={} port={} sslmode=require",
            balancer_pgwire_listen.ip(),
            balancer_pgwire_listen.port()
        ));

        let tls = make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
            Ok(b.set_verify(SslVerifyMode::NONE))
        }));

        let (pg_client, conn) = tokio_postgres::connect(&conn_str, tls.clone())
            .await
            .unwrap();
        task::spawn(|| "balancer-pg_client", async move {
            conn.await.expect("balancer-pg_client")
        });

        let res: i32 = pg_client.query_one("SELECT 2", &[]).await.unwrap().get(0);
        assert_eq!(res, 2);

        // Assert cancellation is propagated.
        let cancel = pg_client.cancel_token();
        let copy = pg_client
            .copy_out("copy (subscribe (select * from mz_kafka_sinks)) to stdout")
            .await
            .unwrap();
        cancel.cancel_query(tls).await.unwrap();
        let e = pin!(copy).next().await.unwrap().unwrap_err();
        assert_contains!(e.to_string(), "canceling statement due to user request");

        if !is_frontegg_resolver {
            continue;
        }

        // Test de-duplication in the frontegg resolver. This is a bit racy so use a retry loop.
        Retry::default()
            .retry_async(|_| async {
                let start_auth_count = *frontegg_server.auth_requests.lock().unwrap();
                const CONNS: u64 = 10;
                let mut handles = Vec::with_capacity(usize::cast_from(CONNS));
                for _ in 0..CONNS {
                    let conn_str = Arc::clone(&conn_str);
                    let handle = task::spawn(|| "test conn", async move {
                        let (pg_client, conn) = tokio_postgres::connect(
                            &conn_str,
                            make_pg_tls(Box::new(|b: &mut SslConnectorBuilder| {
                                Ok(b.set_verify(SslVerifyMode::NONE))
                            })),
                        )
                        .await
                        .unwrap();
                        task::spawn(|| "balancer-pg_client", async move {
                            let _ = conn.await;
                        });
                        let res: i32 = pg_client.query_one("SELECT 2", &[]).await.unwrap().get(0);
                        assert_eq!(res, 2);
                    });
                    handles.push(handle);
                }
                for handle in handles {
                    handle.await.unwrap();
                }
                let end_auth_count = *frontegg_server.auth_requests.lock().unwrap();
                // We expect that the auth count increased by fewer than the number of connections.
                if end_auth_count == start_auth_count + CONNS {
                    // No deduplication was done, try again.
                    return Err("no auth dedup");
                }
                Ok(())
            })
            .await
            .unwrap();
    }
}
