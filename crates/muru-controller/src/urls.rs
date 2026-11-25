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

/// Whether a route reaches `app`: its own application, or a static
/// route's fallback (issue 908); an internal route never does.
fn reaches(route: &Route, app: &str) -> bool {
    route.internal != Some(true) && route.serves_app(app)
}

/// How well one of a host's names or patterns takes `name`, the better
/// the larger, as the edge picks a host (section 4.3): an exact name or
/// the canonical one, then a wildcard (`*.example.com`, one label) by the
/// length of its suffix, then a `~` regular expression; none when it
/// takes no such name.
fn takes(candidate: &str, name: &str) -> Option<(u8, usize)> {
    let candidate = candidate.trim_end_matches('.').to_ascii_lowercase();

    if let Some(pattern) = candidate.strip_prefix('~') {
        let pattern = regex::Regex::new(&format!("^(?:{pattern})$")).ok()?;
        return pattern.is_match(name).then_some((1, 0));
    }

    if let Some(suffix) = candidate.strip_prefix('*') {
        let label = name.strip_suffix(suffix)?;
        return (!label.is_empty() && !label.contains('.')).then_some((2, suffix.len()));
    }

    (candidate == name).then_some((3, 0))
}

/// Whether a host takes requests for `name`: one of its names, patterns
/// or its canonical name does; a default host with no names takes any.
pub fn host_takes(host: &Host, name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let mut names = host.names.iter().chain(host.canonical.as_ref()).peekable();

    if names.peek().is_none() {
        return host.default;
    }

    names.any(|candidate| takes(candidate, &name).is_some())
}

/// The host a request for `name` reaches, as the edge picks it: the one
/// whose name or pattern takes it best, else the default host.
fn host_for<'a>(hosts: &'a [(String, Host)], name: &str) -> Option<&'a str> {
    let name = name.trim_end_matches('.').to_ascii_lowercase();

    hosts
        .iter()
        .filter_map(|(host, settings)| {
            settings
                .names
                .iter()
                .chain(settings.canonical.as_ref())
                .filter_map(|candidate| takes(candidate, &name))
                .max()
                .map(|rank| (rank, host.as_str()))
        })
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, host)| host)
        .or_else(|| {
            hosts
                .iter()
                .find(|(_, settings)| settings.default)
                .map(|(host, _)| host.as_str())
        })
}

/// Why `app` has no URL, for a caller whose candidates came back empty
/// (issue 908): the hosts that route it have names that are all
/// patterns, which a URL cannot carry, or nothing on a listener reaches
/// it at all. `name` is the name asked for, when one was.
pub fn no_address(
    app: &str,
    routes: &[Route],
    hosts: &[(String, Host)],
    name: Option<&str>,
) -> String {
    let routing: Vec<&str> = routes
        .iter()
        .filter(|route| reaches(route, app))
        .map(|route| route.host.as_str())
        .collect();

    if let Some(name) = name {
        let mut names: Vec<&str> = hosts
            .iter()
            .filter(|(host, _)| routing.contains(&host.as_str()))
            .map(|(host, _)| host.as_str())
            .collect();
        names.dedup();

        return match (names.is_empty(), host_for(hosts, name)) {
            (true, _) => format!("{app} has no address: no route on a listener reaches it"),
            (false, Some(other)) => format!(
                "{name} reaches host {other}, which does not route {app}; {app} is routed by {} {}",
                if names.len() == 1 { "host" } else { "hosts" },
                names.join(", ")
            ),
            (false, None) => format!(
                "{name} is not a name of {} {}, which route {app}",
                if names.len() == 1 { "host" } else { "hosts" },
                names.join(", ")
            ),
        };
    }

    let mut patterned: Vec<&str> = hosts
        .iter()
        .filter(|(host, settings)| {
            routing.contains(&host.as_str())
                && !settings.default
                && settings.canonical.is_none()
                && !settings.names.is_empty()
                && settings.names.iter().all(|name| is_pattern(name))
        })
        .map(|(host, _)| host.as_str())
        .collect();
    patterned.dedup();

    if patterned.is_empty() {
        return format!("{app} has no address: no route on a listener reaches it");
    }

    format!(
        "{app} is reached only through {} {}, whose names are all patterns; give the host a \
         canonical name, or pass a name with --name",
        if patterned.len() == 1 {
            "host"
        } else {
            "hosts"
        },
        patterned.join(", ")
    )
}

/// Every URL `app` can be reached at, ranked: its default route, the one
/// with the shortest path, first; a host's canonical name before its
/// other names, wildcard names left out; HTTPS before HTTP; and ties by
/// the listeners' and hosts' order in the configuration, so the choice
/// is always the same. With no host declared, the application is reached
/// on every listener by the listener's address, and so it is through a
/// default host with no names of its own to use (issue 908). A static
/// route whose fallback is the application counts as a route to it at
/// the route's path. With `name`, only the hosts that take that name
/// give URLs, each under that name, whatever names they declare.
pub fn for_app(
    app: &str,
    routes: &[Route],
    hosts: &[(String, Host)],
    listeners: &[(SocketAddr, bool)],
    name: Option<&str>,
) -> Vec<Candidate> {
    let mut found: Vec<(Rank, Candidate)> = Vec::new();

    let scheme = |tls: bool| if tls { "https" } else { "http" };

    if hosts.is_empty() {
        for (index, (address, tls)) in listeners.iter().enumerate() {
            let name = name.map_or_else(|| address_name(*address), str::to_ascii_lowercase);

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

    for route in routes.iter().filter(|route| reaches(route, app)) {
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

        let mut names: Vec<(usize, String, bool)> = host
            .names
            .iter()
            .enumerate()
            .filter(|(_, name)| !is_pattern(name))
            .map(|(index, name)| {
                let canonical = host.canonical.as_deref() == Some(name.as_str());
                (index, name.clone(), canonical)
            })
            .collect();

        if let Some(canonical) = host.canonical.as_deref()
            && !is_pattern(canonical)
            && !names.iter().any(|(_, name, _)| name == canonical)
        {
            names.push((0, canonical.to_owned(), true));
        }

        // A name asked for is the one name of the host it reaches, as the
        // edge picks hosts, and no name of any other.

        if let Some(asked) = name {
            names = if host_for(hosts, asked) == Some(host_name.as_str()) {
                vec![(0, asked.to_ascii_lowercase(), false)]
            } else {
                Vec::new()
            };
        }

        // Section 4.3: a default host takes every request no other host
        // claims, so with no name to use it is reached by each
        // listener's address, as when no host is declared.

        let by_address = names.is_empty() && name.is_none() && host.default;

        for (listener_index, (address, tls)) in used.iter().filter(|_| by_address) {
            let path = route.path.clone();
            let shown = address_name(*address);

            found.push((
                (
                    path.len(),
                    1,
                    u8::from(!tls),
                    *listener_index,
                    host_index,
                    0,
                ),
                Candidate {
                    url: url(scheme(*tls), &shown, address.port(), &path),
                    scheme: scheme(*tls).to_owned(),
                    name: shown,
                    port: address.port(),
                    route: route.name.clone().unwrap_or_default(),
                    host: host_name.clone(),
                    listener: address.to_string(),
                    bound: *address,
                    canonical: false,
                    why: format!(
                        "route {path} on the default host {host_name}, which has no name to \
                         use, listener {address}"
                    ),
                    path,
                },
            ));
        }

        for (name_index, name, canonical) in &names {
            let (name_index, name, canonical) = (*name_index, name.as_str(), *canonical);

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

        let urls: Vec<String> = for_app("shop", &routes, &hosts, &listeners, None)
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

        let urls: Vec<String> =
            for_app("api", &[route("api", "/", "api")], &hosts, &listeners, None)
                .into_iter()
                .map(|candidate| candidate.url)
                .collect();
        assert_eq!(urls, ["http://api.localhost:9000/"]);

        let urls: Vec<String> = for_app("api", &[], &[], &listeners, None)
            .into_iter()
            .map(|candidate| candidate.url)
            .collect();
        assert_eq!(urls, ["http://localhost:8080/", "http://localhost:9000/"]);
    }

    #[test]
    fn a_nameless_default_host_a_static_fallback_and_a_name_asked_for_give_urls() {
        let listeners: Vec<(SocketAddr, bool)> =
            vec![("127.0.0.1:49292".parse().expect("address"), false)];
        let urls = |routes: &[Route], hosts: &[(String, Host)], name: Option<&str>| {
            for_app("echo", routes, hosts, &listeners, name)
                .into_iter()
                .map(|candidate| candidate.url)
                .collect::<Vec<String>>()
        };

        // A default host with no names is reached by the listener's address.

        let local = vec![("local".to_owned(), host("default = true\n"))];
        assert_eq!(
            urls(&[route("local", "/app", "echo")], &local, None),
            ["http://localhost:49292/app"]
        );

        // A static route whose fallback is the application counts, at its
        // path.

        let site = vec![("site".to_owned(), host("names = ['echo.test']\n"))];
        let fallback: Route = toml::from_str(
            "host = 'site'\npath = '/'\nstatic = '/srv/site'\nfallback = { app = 'echo' }\n",
        )
        .expect("a route");
        assert_eq!(
            urls(std::slice::from_ref(&fallback), &site, None),
            ["http://echo.test:49292/"]
        );

        // A host whose names are all patterns gives none without a name,
        // and says why; a name it takes gives its URL, one it does not,
        // none.

        let wild = vec![("wild".to_owned(), host("names = ['*.echo.test']\n"))];
        let routes = [route("wild", "/app", "echo")];
        assert!(urls(&routes, &wild, None).is_empty());
        assert_eq!(
            no_address("echo", &routes, &wild, None),
            "echo is reached only through host wild, whose names are all patterns; give the \
             host a canonical name, or pass a name with --name"
        );
        assert_eq!(
            urls(&routes, &wild, Some("alice.echo.test")),
            ["http://alice.echo.test:49292/app"]
        );
        assert!(urls(&routes, &wild, Some("a.b.echo.test")).is_empty());
        assert_eq!(
            no_address("echo", &routes, &wild, Some("other.test")),
            "other.test is not a name of host wild, which route echo"
        );

        let pattern = host("names = ['~(alice|bob)\\.echo\\.test']\n");
        assert!(host_takes(&pattern, "bob.echo.test"));
        assert!(!host_takes(&pattern, "carol.echo.test"));
        assert!(host_takes(&host("default = true\n"), "anything.test"));
        assert_eq!(
            no_address("echo", &[], &site, None),
            "echo has no address: no route on a listener reaches it"
        );
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
