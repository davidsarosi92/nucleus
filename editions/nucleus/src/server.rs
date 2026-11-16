//! `nucleus server`: the front ends a command line names — the nucleus
//! protocol by default, PostgreSQL compatibility (the `pg-compat` module,
//! which this edition includes) on request; TLS (the `tls` module) for
//! clients that ask, given a certificate and its key.

use std::net::TcpListener;
use std::sync::Arc;

use nucleus_kv::{Kv, Options};
use nucleus_server::{
    Config, DEFAULT_PASSWORD, DEFAULT_USER, FrontEnds, Password, Trust, serve_front_end,
};

pub fn run(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first() else {
        return Err("server: missing <dir>".into());
    };
    let mut fronts = FrontEnds::builtin();
    fronts.add(Arc::new(nucleus_pg_compat::Pg))?;
    fronts.add(Arc::new(nucleus_console::Console::new()))?;
    // (front end, address): ncp always, on `--listen`'s address.
    let mut listen: Option<String> = None;
    let mut extra: Vec<(String, Option<String>)> = Vec::new();
    // NUCLEUS_PASSWORD: --password's default (a container's settings come
    // as its environment).
    let mut password = Some(
        std::env::var("NUCLEUS_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_PASSWORD.to_string()),
    );
    let mut user = DEFAULT_USER.to_string();
    let mut log_queries = false;
    let mut insecure = false;
    let (mut tls_cert, mut tls_key, mut tls_client_ca) = (None, None, None::<String>);
    let (mut oidc_issuer, mut oidc_audience, mut oidc_jwks) = (None, None, None::<String>);
    let (mut oidc_user_claim, mut oidc_role_claim) = (None::<String>, None::<String>);
    let mut ldap: std::collections::BTreeMap<&str, String> = Default::default();
    let mut trusted_proxies = Vec::new();
    let mut max_connections = None;
    let mut sync = nucleus_kv::SyncPolicy::default();
    let mut wal_minimal = true;
    let mut cache_bytes = None;
    let mut history_time = std::time::Duration::ZERO;
    let mut secrets_key = None::<String>;
    let mut subject_keys = None::<String>;
    let mut key_options = crate::keys::KeyOptions::default();
    let mut metrics = None::<String>;
    let mut otlp = None::<String>;
    let mut backup_key = None::<String>;
    let mut backup_to = None::<String>;
    let mut backup_every = None::<std::time::Duration>;
    let mut backup_full_every = 7u64;
    let mut backup_keep = None::<u64>;
    let mut restore_test_every = None::<std::time::Duration>;
    let mut restore_test_dir = None::<String>;
    let mut rest = args[1..].iter().peekable();
    // An optional address after a flag.
    let addr = |rest: &mut std::iter::Peekable<std::slice::Iter<'_, String>>| match rest.peek() {
        Some(next) if !next.starts_with("--") => rest.next().cloned(),
        _ => None,
    };
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--listen" => listen = Some(rest.next().ok_or("--listen needs an address")?.clone()),
            "--pg-compat" => extra.push(("pg".into(), addr(&mut rest))),
            "--console" => extra.push(("console".into(), addr(&mut rest))),
            "--otlp" => otlp = Some(rest.next().ok_or("--otlp needs an endpoint")?.clone()),
            "--metrics" => {
                metrics =
                    Some(addr(&mut rest).unwrap_or_else(|| nucleus_prometheus::DEFAULT_ADDR.into()))
            }
            "--front-end" => {
                let name = rest.next().ok_or("--front-end needs a name")?.clone();
                extra.push((name, addr(&mut rest)));
            }
            "--user" => user = rest.next().ok_or("--user needs a name")?.clone(),
            "--password" => password = Some(rest.next().ok_or("--password needs a value")?.clone()),
            "--no-auth" => password = None,
            "--sync" => {
                sync = match rest.next().map(String::as_str) {
                    Some("ordered") => nucleus_kv::SyncPolicy::Ordered,
                    Some("full") => nucleus_kv::SyncPolicy::Full,
                    _ => return Err("--sync takes ordered or full".into()),
                }
            }
            "--wal" => {
                wal_minimal = match rest.next().map(String::as_str) {
                    Some("minimal") => true,
                    Some("full") => false,
                    _ => return Err("--wal takes minimal or full".into()),
                }
            }
            "--cache" => {
                let v = rest.next().ok_or("--cache needs a size")?;
                cache_bytes = Some(nucleus_sql::dbsettings::memory(v).map_err(|e| e.message)?);
            }
            "--history" => {
                let v = rest.next().ok_or("--history needs a duration")?;
                let us = nucleus_sql::dbsettings::duration(v).map_err(|e| e.message)?;
                history_time = std::time::Duration::from_micros(us as u64);
            }
            "--index-cache" => {
                let v = rest.next().ok_or("--index-cache needs a size")?;
                nucleus_sql::set_index_cache(
                    nucleus_sql::dbsettings::memory(v).map_err(|e| e.message)?,
                );
            }
            "--log-queries" => log_queries = true,
            k if key_options.take(k, &mut rest)? => {}
            "--secrets-key" => {
                secrets_key = Some(rest.next().ok_or("--secrets-key needs a file")?.clone())
            }
            "--subject-keys" => {
                subject_keys = Some(rest.next().ok_or("--subject-keys needs a file")?.clone())
            }
            "--backup-key" => {
                backup_key = Some(rest.next().ok_or("--backup-key needs a file")?.clone())
            }
            "--backup-to" => {
                backup_to = Some(rest.next().ok_or("--backup-to needs a repository")?.clone())
            }
            "--backup-every" => {
                let v = rest.next().ok_or("--backup-every needs a duration")?;
                let us = nucleus_sql::dbsettings::duration(v).map_err(|e| e.message)?;
                if us < 1_000_000 {
                    return Err(format!("--backup-every {v}: at least a second"));
                }
                backup_every = Some(std::time::Duration::from_micros(us as u64));
            }
            "--backup-full-every" => {
                let n = rest.next().ok_or("--backup-full-every needs a number")?;
                backup_full_every = n
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--backup-full-every {n}: a number above 0"))?;
            }
            "--restore-test-every" => {
                let v = rest.next().ok_or("--restore-test-every needs a duration")?;
                let us = nucleus_sql::dbsettings::duration(v).map_err(|e| e.message)?;
                if us < 1_000_000 {
                    return Err(format!("--restore-test-every {v}: at least a second"));
                }
                restore_test_every = Some(std::time::Duration::from_micros(us as u64));
            }
            "--restore-test-dir" => {
                restore_test_dir = Some(
                    rest.next()
                        .ok_or("--restore-test-dir needs a directory")?
                        .clone(),
                )
            }
            "--backup-keep" => {
                let n = rest.next().ok_or("--backup-keep needs a number")?;
                backup_keep = Some(
                    n.parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("--backup-keep {n}: a number above 0"))?,
                );
            }
            "--insecure" => insecure = true,
            "--tls-cert" => tls_cert = Some(rest.next().ok_or("--tls-cert needs a file")?.clone()),
            "--tls-key" => tls_key = Some(rest.next().ok_or("--tls-key needs a file")?.clone()),
            "--oidc-issuer" => {
                oidc_issuer = Some(rest.next().ok_or("--oidc-issuer needs a URL")?.clone())
            }
            "--oidc-audience" => {
                oidc_audience = Some(rest.next().ok_or("--oidc-audience needs a name")?.clone())
            }
            "--oidc-jwks" => {
                oidc_jwks = Some(rest.next().ok_or("--oidc-jwks needs a file")?.clone())
            }
            "--oidc-user-claim" => {
                oidc_user_claim = Some(
                    rest.next()
                        .ok_or("--oidc-user-claim needs a claim")?
                        .clone(),
                )
            }
            "--oidc-role-claim" => {
                oidc_role_claim = Some(
                    rest.next()
                        .ok_or("--oidc-role-claim needs a claim")?
                        .clone(),
                )
            }
            "--ldap-url"
            | "--ldap-bind-dn"
            | "--ldap-search-base"
            | "--ldap-search-attribute"
            | "--ldap-search-dn"
            | "--ldap-search-password"
            | "--ldap-ca" => {
                let v = rest
                    .next()
                    .ok_or_else(|| format!("{a} needs a value"))?
                    .clone();
                ldap.insert(&a[7..], v);
            }
            "--max-connections" => {
                let n = rest.next().ok_or("--max-connections needs a number")?;
                max_connections = Some(
                    n.parse()
                        .map_err(|_| format!("--max-connections {n}: not a number"))?,
                );
            }
            "--trusted-proxy" => {
                let cidr = rest
                    .next()
                    .ok_or("--trusted-proxy needs an address (CIDR)")?;
                nucleus_sql::inet::parse_cidr(cidr)
                    .map_err(|e| format!("--trusted-proxy {cidr}: {}", e.message))?;
                trusted_proxies.push(cidr.clone());
            }
            "--ldap-starttls" => {
                ldap.insert("starttls", String::new());
            }
            "--tls-client-ca" => {
                tls_client_ca = Some(rest.next().ok_or("--tls-client-ca needs a file")?.clone())
            }
            other => return Err(format!("server: unknown option {other}")),
        }
    }
    let schedule = match (backup_to, backup_every) {
        (Some(to), Some(every)) => {
            // A path made absolute (BACKUP TO's: on the server).
            let to = if to.contains("://") {
                to
            } else {
                std::path::absolute(&to)
                    .map_err(|e| format!("--backup-to {to}: {e}"))?
                    .to_string_lossy()
                    .into_owned()
            };
            Some(nucleus_server::backups::Schedule {
                to,
                every,
                full_every: backup_full_every,
                keep: backup_keep,
            })
        }
        (None, None) if backup_keep.is_none() => None,
        _ => return Err("server: --backup-to and --backup-every go together".into()),
    };
    let restore_tests = match (&schedule, restore_test_every) {
        (Some(s), Some(every)) => {
            let scratch = restore_test_dir.map_or_else(
                || format!("{dir}.restore-test").into(),
                std::path::PathBuf::from,
            );
            nucleus_server::backups::claim_scratch(&scratch)
                .map_err(|e| format!("server: --restore-test-dir: {e}"))?;
            Some(nucleus_server::backups::RestoreTests {
                to: s.to.clone(),
                every,
                scratch,
            })
        }
        (None, Some(_)) => {
            return Err(
                "server: --restore-test-every tests --backup-to's backups: give both".into(),
            );
        }
        (_, None) if restore_test_dir.is_some() => {
            return Err("server: --restore-test-dir goes with --restore-test-every".into());
        }
        (_, None) => None,
    };
    let backup_key_id = nucleus_server::backups::load_key(backup_key.as_deref())?;
    let weak_password = password.as_deref() == Some(DEFAULT_PASSWORD);
    let no_auth = password.is_none();
    let shown_user = user.clone();
    let config = Config {
        auth: match password {
            Some(password) => Arc::new(Password { user, password }),
            None => Arc::new(Trust),
        },
        log_queries,
        registry: crate::sql::registry(),
        tls: match (tls_cert, tls_key) {
            (Some(cert), Some(key)) => {
                let tls = nucleus_tls::ServerTls::from_pem_files(cert.as_ref(), key.as_ref())
                    .map_err(|e| format!("server: TLS: {e}"))?;
                let tls = match &tls_client_ca {
                    Some(ca) => tls
                        .accepting_client_certificates_file(ca.as_ref())
                        .map_err(|e| format!("server: --tls-client-ca: {e}"))?,
                    None => tls,
                };
                Some(Arc::new(tls))
            }
            (None, None) if tls_client_ca.is_some() => {
                return Err("server: --tls-client-ca needs --tls-cert and --tls-key".into());
            }
            (None, None) => None,
            _ => return Err("server: --tls-cert and --tls-key go together".into()),
        },
        bearer: match (oidc_issuer, oidc_audience) {
            (Some(issuer), Some(audience)) => {
                let keys = match &oidc_jwks {
                    Some(file) => nucleus_oidc::Keys::Static(
                        std::fs::read_to_string(file)
                            .map_err(|e| format!("--oidc-jwks {file}: {e}"))
                            .and_then(|t| nucleus_oidc::Jwks::from_json(&t))
                            .map_err(|e| format!("server: {e}"))?,
                    ),
                    None => nucleus_oidc::Keys::Discover,
                };
                let mut oidc = nucleus_oidc::Oidc::new(&issuer, &audience, keys);
                if let Some(c) = &oidc_user_claim {
                    oidc = oidc.user_claim(c);
                }
                if let Some(c) = &oidc_role_claim {
                    oidc = oidc.role_claim(c);
                }
                Some(Arc::new(oidc) as Arc<dyn nucleus_server::Bearer>)
            }
            (None, None) if oidc_jwks.is_none() => None,
            _ => return Err("server: --oidc-issuer and --oidc-audience go together".into()),
        },
        directory: match ldap.get("url") {
            None if ldap.is_empty() => None,
            None => return Err("server: the --ldap-* options need --ldap-url".into()),
            Some(url) => {
                let mut d = match (ldap.get("bind-dn"), ldap.get("search-base")) {
                    (Some(t), None) => nucleus_ldap::Ldap::bind(url, t),
                    (None, Some(base)) => nucleus_ldap::Ldap::search(
                        url,
                        base,
                        ldap.get("search-attribute").map_or("uid", String::as_str),
                    ),
                    _ => Err("give --ldap-bind-dn or --ldap-search-base".into()),
                }
                .map_err(|e| format!("server: LDAP: {e}"))?;
                if let (Some(dn), Some(pw)) = (ldap.get("search-dn"), ldap.get("search-password")) {
                    d = d.with_search_account(dn, pw);
                }
                if ldap.contains_key("starttls") {
                    d = d.starttls();
                }
                if let Some(ca) = ldap.get("ca") {
                    d = d
                        .with_ca_file(ca.as_ref())
                        .map_err(|e| format!("server: --ldap-ca: {e}"))?;
                }
                Some(Arc::new(d) as Arc<dyn nucleus_server::Directory>)
            }
        },
        trusted_proxies,
        max_connections,
        throttle: Default::default(),
    };
    let defaults = Options::default();
    let opts = Options {
        sync,
        wal_minimal,
        cache_bytes: cache_bytes.unwrap_or(defaults.cache_bytes),
        history_time,
        ..defaults
    };
    // The metrics and health checks (PLAN.md §M14.18) before the
    // database: a probe sees a long recovery as alive, not ready.  Every
    // layer's instruments there from the first scrape.
    nucleus_sql::metrics::register();
    let metrics_addr = match metrics {
        Some(addr) => {
            let l = TcpListener::bind(&addr)
                .map_err(|e| format!("cannot listen on {addr} (metrics): {e}"))?;
            let a = l.local_addr().map_err(|e| e.to_string())?;
            if !a.ip().is_loopback() && config.tls.is_none() && !insecure {
                return Err(format!(
                    "server: refusing to serve metrics on {a} over plain HTTP: give --tls-cert and --tls-key, listen on loopback, or say --insecure"
                ));
            }
            let exporter = nucleus_prometheus::Prometheus::new(l, config.tls.clone());
            nucleus_platform::metrics::Exporter::start(
                Box::new(exporter),
                nucleus_platform::metrics::global(),
            )
            .map_err(|e| format!("metrics: {e}"))?;
            Some(a)
        }
        None => None,
    };
    // OpenTelemetry: pushed to a collector (--otlp or the SDKs' variables).
    let otlp = nucleus_otlp::Otlp::from_env(otlp)?;
    let otlp_shown = otlp
        .as_ref()
        .map(|o| format!("{} every {}s", o.endpoint, o.interval.as_secs_f64()));
    if let Some(o) = otlp {
        nucleus_platform::metrics::Exporter::start(
            Box::new(o),
            nucleus_platform::metrics::global(),
        )
        .map_err(|e| format!("otlp: {e}"))?;
    }
    // Under its data key when it is encrypted (PLAN.md §M14.30 K2e).
    let db = crate::keys::open_database(dir, &key_options.settings(dir), opts)?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    // The key hierarchy (PLAN.md §M14.30 K1): the keyring, the secrets'
    // key in it.
    crate::keys::install(
        dir,
        &kv,
        key_options.settings(dir),
        secrets_key.map(Into::into),
        subject_keys.map(Into::into),
    )?;
    let mut plan = vec![("ncp".to_string(), listen)];
    plan.extend(extra);
    let mut listeners = Vec::new();
    for (name, addr) in plan {
        let front = fronts.get(&name).ok_or_else(|| {
            format!(
                "server: no front end {name} (there are: {})",
                fronts.names().join(", ")
            )
        })?;
        let addr = addr.unwrap_or_else(|| front.default_addr());
        let l = TcpListener::bind(&addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        listeners.push((Arc::clone(front), l));
    }
    // Beyond loopback, not with the default password (while the database
    // is unclaimed and so takes it) or without authentication — unless told.
    let exposed: Vec<String> = listeners
        .iter()
        .filter_map(|(_, l)| l.local_addr().ok())
        .filter(|a| !a.ip().is_loopback())
        .map(|a| a.to_string())
        .collect();
    if !exposed.is_empty() && !insecure {
        let unclaimed = || -> Result<bool, String> {
            let txn = kv
                .begin(nucleus_kv::TxnOptions::read_only())
                .map_err(|e| e.to_string())?;
            nucleus_sql::roles::unclaimed(&*txn).map_err(|e| e.message)
        };
        // The console's page sends its password in the request: beyond
        // loopback, over TLS only.
        let console_plain = config.tls.is_none()
            && listeners.iter().any(|(f, l)| {
                f.name() == "console" && l.local_addr().is_ok_and(|a| !a.ip().is_loopback())
            });
        const CLAIM: &str = "claim the database (nucleus init), give --password, or say --insecure";
        let why = if no_auth {
            Some(("without authentication (--no-auth)", CLAIM))
        } else if console_plain {
            Some((
                "with the console over plain HTTP (its page sends the password)",
                "give --tls-cert and --tls-key, listen on loopback, or say --insecure",
            ))
        } else if weak_password && unclaimed()? {
            Some((
                "with the default password while the database is unclaimed",
                CLAIM,
            ))
        } else {
            None
        };
        if let Some((why, what)) = why {
            return Err(format!(
                "server: refusing to listen on {} {why}: {what}",
                exposed.join(", ")
            ));
        }
    }
    let mut shown: Vec<String> = listeners
        .iter()
        .map(|(f, l)| {
            let what = match f.name() {
                "ncp" => "nucleus protocol",
                "pg" => "PostgreSQL compatibility",
                "console" => "web console",
                other => other,
            };
            l.local_addr()
                .map(|a| format!("{a} ({what})"))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    if let Some(a) = metrics_addr {
        shown.push(format!("{a} (metrics and health checks)"));
    }
    // Recovered, open, listening (connections wait in the listeners'
    // backlogs until they are served): ready (`/health/ready`,
    // `nucleus_ready`) — before the line saying so, which whoever started
    // the server may act on at once.
    nucleus_platform::metrics::set_ready(true);
    nucleus_platform::log!(
        "nucleus {} listening on {} (database {dir}; index cache {} MB; {}{})",
        env!("CARGO_PKG_VERSION"),
        shown.join(" and "),
        nucleus_sql::index_cache() >> 20,
        config.auth.describe(),
        match (&config.tls, &config.bearer) {
            (Some(_), Some(b)) => format!("; TLS offered; {}", b.describe()),
            (Some(_), None) => "; TLS offered".to_string(),
            (None, Some(b)) => format!("; {}", b.describe()),
            (None, None) => String::new(),
        } + &config
            .directory
            .as_ref()
            .map_or(String::new(), |d| format!("; {}", d.describe()))
    );
    // How to connect, for whoever started it.
    let local = |a: std::net::SocketAddr| {
        if a.ip().is_unspecified() {
            std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port())
        } else {
            a
        }
    };
    let hints: Vec<String> = listeners
        .iter()
        .filter_map(|(f, l)| Some((f.name(), local(l.local_addr().ok()?))))
        .filter_map(|(name, a)| match name {
            "ncp" if a.to_string() == "127.0.0.1:20035" => Some("nucleus sql".to_string()),
            "ncp" => Some(format!("nucleus sql -h {a}")),
            "console" => Some(format!(
                "{}://{a}/ (the console)",
                if config.tls.is_some() {
                    "https"
                } else {
                    "http"
                }
            )),
            "pg" => Some(format!(
                "psql -h {} -p {} -U {shown_user}",
                a.ip(),
                a.port()
            )),
            _ => None,
        })
        .collect();
    if let Some(o) = otlp_shown {
        nucleus_platform::log!("metrics: OTLP to {o}");
    }
    if !hints.is_empty() {
        nucleus_platform::log!("connect: {}", hints.join("  or  "));
    }
    nucleus_server::maintain_with(
        Arc::clone(&kv),
        nucleus_server::Stop::new(),
        Arc::clone(&config.registry),
    );
    if let Some(s) = schedule {
        nucleus_platform::log!(
            "backups: to {} every {}s, a full one every {}{}{}",
            s.to,
            s.every.as_secs(),
            s.full_every,
            s.keep
                .map_or(String::new(), |k| format!(", the last {k} full ones kept")),
            backup_key_id
                .as_ref()
                .map_or(String::new(), |id| format!(", encrypted (key {id})"))
        );
        nucleus_server::backups::start(Arc::clone(&kv), s, nucleus_server::Stop::new());
    }
    if let Some(t) = restore_tests {
        nucleus_platform::log!(
            "restore tests: of {} every {}s, in {}",
            t.to,
            t.every.as_secs(),
            t.scratch.display()
        );
        nucleus_server::backups::start_restore_tests(
            Arc::clone(&kv),
            Arc::clone(&config.registry),
            t,
            nucleus_server::Stop::new(),
        );
    }
    // The embedding columns' jobs (§M14.22 E4b), off the sessions.
    nucleus_sql::embedding::serve(Arc::clone(&kv), Arc::clone(&config.registry));
    let mut listeners = listeners.into_iter();
    let (first, first_listener) = listeners.next().expect("ncp");
    for (front, l) in listeners {
        let (kv, config) = (Arc::clone(&kv), config.clone());
        std::thread::spawn(move || {
            let name = front.name().to_string();
            if let Err(e) = serve_front_end(front, l, kv, config) {
                nucleus_platform::log!("nucleus: {name} listener stopped: {e}");
            }
        });
    }
    serve_front_end(first, first_listener, kv, config).map_err(|e| e.to_string())
}
