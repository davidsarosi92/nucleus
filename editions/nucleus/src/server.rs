//! `nucleus server`: the front ends a command line names — the nucleus
//! protocol by default, PostgreSQL compatibility (the `pg-compat` module,
//! which this edition includes) on request; TLS (the `tls` module) for
//! clients that ask, given a certificate and its key.

use std::net::TcpListener;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_server::{
    Config, DEFAULT_PASSWORD, DEFAULT_USER, FrontEnds, Password, Trust, serve_front_end,
};

pub fn run(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first() else {
        return Err("server: missing <dir>".into());
    };
    let mut fronts = FrontEnds::builtin();
    fronts.add(Arc::new(nucleus_pg_compat::Pg))?;
    // (front end, address): ncp always, on `--listen`'s address.
    let mut listen: Option<String> = None;
    let mut extra: Vec<(String, Option<String>)> = Vec::new();
    let (mut user, mut password) = (DEFAULT_USER.to_string(), Some(DEFAULT_PASSWORD.to_string()));
    let mut log_queries = false;
    let mut insecure = false;
    let (mut tls_cert, mut tls_key, mut tls_client_ca) = (None, None, None::<String>);
    let (mut oidc_issuer, mut oidc_audience, mut oidc_jwks) = (None, None, None::<String>);
    let (mut oidc_user_claim, mut oidc_role_claim) = (None::<String>, None::<String>);
    let mut ldap: std::collections::BTreeMap<&str, String> = Default::default();
    let mut trusted_proxies = Vec::new();
    let mut max_connections = None;
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
            "--front-end" => {
                let name = rest.next().ok_or("--front-end needs a name")?.clone();
                extra.push((name, addr(&mut rest)));
            }
            "--user" => user = rest.next().ok_or("--user needs a name")?.clone(),
            "--password" => password = Some(rest.next().ok_or("--password needs a value")?.clone()),
            "--no-auth" => password = None,
            "--log-queries" => log_queries = true,
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
    let weak_password = password.as_deref() == Some(DEFAULT_PASSWORD);
    let no_auth = password.is_none();
    let config = Config {
        auth: match password {
            Some(password) => Arc::new(Password { user, password }),
            None => Arc::new(Trust),
        },
        log_queries,
        registry: nucleus_sql::ext::Registry::shared_builtin(),
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
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
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
        let why = if no_auth {
            Some("without authentication (--no-auth)")
        } else if weak_password && unclaimed()? {
            Some("with the default password while the database is unclaimed")
        } else {
            None
        };
        if let Some(why) = why {
            return Err(format!(
                "server: refusing to listen on {} {why}: claim the database (nucleus init), \
                 give --password, or say --insecure",
                exposed.join(", ")
            ));
        }
    }
    let shown: Vec<String> = listeners
        .iter()
        .map(|(f, l)| {
            let what = match f.name() {
                "ncp" => "nucleus protocol",
                "pg" => "PostgreSQL compatibility",
                other => other,
            };
            l.local_addr()
                .map(|a| format!("{a} ({what})"))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    eprintln!(
        "nucleus {} listening on {} (database {dir}; {}{})",
        env!("CARGO_PKG_VERSION"),
        shown.join(" and "),
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
    let mut listeners = listeners.into_iter();
    let (first, first_listener) = listeners.next().expect("ncp");
    for (front, l) in listeners {
        let (kv, config) = (Arc::clone(&kv), config.clone());
        std::thread::spawn(move || {
            let name = front.name().to_string();
            if let Err(e) = serve_front_end(front, l, kv, config) {
                eprintln!("nucleus: {name} listener stopped: {e}");
            }
        });
    }
    serve_front_end(first, first_listener, kv, config).map_err(|e| e.to_string())
}
