//! Where an application on a daemon can be reached (design specification
//! sections 4.3 and 10.6, issue 815): every URL its routes give it, on
//! each virtual host a route belongs to, under each of the host's names,
//! on each listener the host uses, best first. `muructl open`, the
//! dashboard's application rows and the MCP `urls` tool all read it
//! through the admin API's ApplicationUrls, so the client repeats none of
//! the routing.

use std::net::SocketAddr;

use muru_config::{Host, Route};

/// One address an application can be reached at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The URL.
    pub url: String,
    /// `http` or `https`.
    pub scheme: String,
    /// The name in the URL: a virtual host's, or the listener's address.
    pub name: String,
    /// The port.
    pub port: u16,
    /// The route's path.
    pub path: String,
    /// The route's name, or empty.
    pub route: String,
    /// The virtual host, or empty when none is declared.
    pub host: String,
    /// The listener, as its address.
    pub listener: String,
    /// The address the listener is bound on.
    pub bound: SocketAddr,
    /// Whether the name is the host's canonical one.
    pub canonical: bool,
    /// How it was chosen, as a phrase.
    pub why: String,
}

/// Whether a host name is a pattern rather than a name a browser can open.
fn is_pattern(name: &str) -> bool {
    name.starts_with('*') || name.starts_with('~') || name.contains('*')
}

/// The name a URL carries for an address: `localhost` for a loopback or
/// wildcard bind, else the address, in brackets for IPv6.
pub fn address_name(address: SocketAddr) -> String {
    let ip = address.ip();

    if ip.is_loopback() || ip.is_unspecified() {
        return "localhost".to_owned();
    }

    match ip {
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        std::net::IpAddr::V4(ip) => ip.to_string(),
    }
}

/// A URL from its parts, leaving out the scheme's default port.
pub fn url(scheme: &str, name: &str, port: u16, path: &str) -> String {
    let default = matches!((scheme, port), ("http", 80) | ("https", 443));

    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };

    if default {
        format!("{scheme}://{name}{path}")
    } else {
        format!("{scheme}://{name}:{port}{path}")
    }
}

/// Where a candidate ranks, the smaller first: the route's path length,
/// not the canonical name, not TLS, then the listener's, host's and
/// name's places in the configuration.
type Rank = (usize, u8, u8, usize, usize, usize);

/// Every URL `app` can be reached at, ranked: its default route, the one
/// with the shortest path, first; a host's canonical name before its
/// other names, wildcard names left out; HTTPS before HTTP; and ties by
/// the listeners' and hosts' order in the configuration, so the choice
/// is always the same. With no host declared, the application is reached
/// on every listener by the listener's address.
pub fn for_app(
    app: &str,
    routes: &[Route],
    hosts: &[(String, Host)],
    listeners: &[(SocketAddr, bool)],
) -> Vec<Candidate> {
    let mut found: Vec<(Rank, Candidate)> = Vec::new();

    let scheme = |tls: bool| if tls { "https" } else { "http" };

    if hosts.is_empty() {
        for (index, (address, tls)) in listeners.iter().enumerate() {
            let name = address_name(*address);

            found.push((
                (1, 0, u8::from(!tls), index, 0, 0),
                Candidate {
                    url: url(scheme(*tls), &name, address.port(), "/"),
                    scheme: scheme(*tls).to_owned(),
                    name,
                    port: address.port(),
                    path: "/".to_owned(),
                    route: String::new(),
                    host: String::new(),
                    listener: address.to_string(),
                    bound: *address,
                    canonical: false,
                    why: format!("no host is declared, listener {address}"),
                },
            ));
        }
    }

    for route in routes
        .iter()
        .filter(|route| route.app == app && route.internal != Some(true))
    {
        let Some((host_index, (host_name, host))) = hosts
            .iter()
            .enumerate()
            .find(|(_, (name, _))| *name == route.host)
        else {
            continue;
        };

        // The listeners the host answers on: those it names, or every one.

        let used: Vec<(usize, &(SocketAddr, bool))> = listeners
            .iter()
            .enumerate()
            .filter(|(_, (address, _))| {
                host.listeners.is_empty()
                    || host
                        .listeners
                        .iter()
                        .any(|named| named.parse::<SocketAddr>().ok() == Some(*address))
            })
            .collect();

        let mut names: Vec<(usize, &str, bool)> = host
            .names
            .iter()
            .enumerate()
            .filter(|(_, name)| !is_pattern(name))
            .map(|(index, name)| {
                let canonical = host.canonical.as_deref() == Some(name.as_str());
                (index, name.as_str(), canonical)
            })
            .collect();

        if let Some(canonical) = host.canonical.as_deref()
            && !is_pattern(canonical)
            && !names.iter().any(|(_, name, _)| *name == canonical)
        {
            names.push((0, canonical, true));
        }

        for (name_index, name, canonical) in names {
            for (listener_index, (address, tls)) in &used {
                let path = route.path.clone();

                found.push((
                    (
                        path.len(),
                        u8::from(!canonical),
                        u8::from(!tls),
                        *listener_index,
                        host_index,
                        name_index,
                    ),
                    Candidate {
                        url: url(scheme(*tls), name, address.port(), &path),
                        scheme: scheme(*tls).to_owned(),
                        name: name.to_owned(),
                        port: address.port(),
                        route: route.name.clone().unwrap_or_default(),
                        host: host_name.clone(),
                        listener: address.to_string(),
                        bound: *address,
                        canonical,
                        why: format!(
                            "route {path}{} on host {host_name}, {}name {name}, listener {address}",
                            route
                                .name
                                .as_deref()
                                .map(|name| format!(" ({name})"))
                                .unwrap_or_default(),
                            if canonical { "canonical " } else { "" }
                        ),
                        path,
                    },
                ));
            }
        }
    }

    found.sort_by_key(|(rank, _)| *rank);

    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .map(|(_, candidate)| candidate)
        .filter(|candidate| seen.insert(candidate.url.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: &str, path: &str, app: &str) -> Route {
        toml::from_str(&format!(
            "host = '{host}'\npath = '{path}'\napp = '{app}'\n"
        ))
        .expect("a route")
    }

    fn host(text: &str) -> Host {
        toml::from_str(text).expect("a host")
    }

    #[test]
    fn the_default_route_canonical_name_and_https_come_first() {
        let listeners: Vec<(SocketAddr, bool)> = vec![
            ("0.0.0.0:80".parse().expect("address"), false),
            ("0.0.0.0:443".parse().expect("address"), true),
        ];
        let hosts = vec![(
            "shop".to_owned(),
            host(
                "names = ['www.shop.localhost', 'shop.localhost', '*.shop.localhost']\ncanonical = 'shop.localhost'\n",
            ),
        )];
        let routes = vec![
            route("shop", "/admin/", "shop"),
            route("shop", "/", "shop"),
            route("shop", "/", "blog"),
        ];

        let urls: Vec<String> = for_app("shop", &routes, &hosts, &listeners)
            .into_iter()
            .map(|candidate| candidate.url)
            .collect();

        assert_eq!(
            urls,
            [
                "https://shop.localhost/",
                "http://shop.localhost/",
                "https://www.shop.localhost/",
                "http://www.shop.localhost/",
                "https://shop.localhost/admin/",
                "http://shop.localhost/admin/",
                "https://www.shop.localhost/admin/",
                "http://www.shop.localhost/admin/",
            ]
        );
    }

    #[test]
    fn a_host_answers_on_its_own_listeners_and_no_host_means_every_listener() {
        let listeners: Vec<(SocketAddr, bool)> = vec![
            ("127.0.0.1:8080".parse().expect("address"), false),
            ("127.0.0.1:9000".parse().expect("address"), false),
        ];
        let hosts = vec![(
            "api".to_owned(),
            host("names = ['api.localhost']\nlisteners = ['127.0.0.1:9000']\n"),
        )];

        let urls: Vec<String> = for_app("api", &[route("api", "/", "api")], &hosts, &listeners)
            .into_iter()
            .map(|candidate| candidate.url)
            .collect();
        assert_eq!(urls, ["http://api.localhost:9000/"]);

        let urls: Vec<String> = for_app("api", &[], &[], &listeners)
            .into_iter()
            .map(|candidate| candidate.url)
            .collect();
        assert_eq!(urls, ["http://localhost:8080/", "http://localhost:9000/"]);
    }

    #[test]
    fn urls_bracket_ipv6_and_leave_out_default_ports() {
        assert_eq!(
            address_name("[::1]:8080".parse().expect("address")),
            "localhost"
        );
        assert_eq!(
            address_name("[2001:db8::1]:8080".parse().expect("address")),
            "[2001:db8::1]"
        );
        assert_eq!(
            url("https", "shop.localhost", 443, "/"),
            "https://shop.localhost/"
        );
        assert_eq!(
            url("http", "[2001:db8::1]", 8080, "x"),
            "http://[2001:db8::1]:8080/x"
        );
    }
}
