//! What stands in for a secret in an archive: a value whose name says it
//! is one is masked whole, and a password in a URL is masked alone, as
//! `muructl debug inspect` shows a worker's environment (section 11.7).

/// What a masked value reads as.
pub const MASK: &str = "***";

/// The words that mark a name as a secret's, wherever in it they are.
const SECRET_WORDS: [&str; 7] = [
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "KEY",
    "CREDENTIAL",
    "AUTH",
];

/// Whether a variable's or a setting's name says its value is a secret.
pub fn secret_name(name: &str) -> bool {
    let name = name.to_uppercase();
    name.ends_with("_DSN") || SECRET_WORDS.iter().any(|word| name.contains(word))
}

/// Whether a setting's key says its value is a secret. A key is read
/// word by word, since a setting is named in words where a variable is
/// not: `secret`, `token`, `password` and the like anywhere in it, or
/// `key` at its end after another word, as `signing_key`. A `key` alone
/// names what something is keyed by, and so does `hash_key`.
pub fn secret_setting(key: &str) -> bool {
    let key = key.to_lowercase();
    let words: Vec<&str> = key
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();

    let named = words.iter().any(|word| {
        [
            "secret",
            "secrets",
            "token",
            "password",
            "passwd",
            "credential",
            "credentials",
            "auth",
            "dsn",
            "apikey",
        ]
        .contains(word)
    });

    named || (words.len() > 1 && words.last() == Some(&"key") && words[0] != "hash")
}

/// A value as an archive holds it: masked whole under a name that says
/// it is a secret, and otherwise with the password of any URL in it
/// masked.
pub fn value(name: &str, value: &str) -> String {
    if secret_name(name) {
        MASK.to_owned()
    } else {
        url_passwords(value)
    }
}

/// A text with the password of each URL in it masked, the rest of the
/// URL kept, so that `postgres://shop:hunter2@db/shop` still names its
/// database and host. A value may list several URLs, separated by
/// commas, semicolons or spaces.
pub fn url_passwords(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(end) = rest.find(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let separator = rest[end..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&url_password(&rest[..end]));
        out.push_str(&rest[end..end + separator]);
        rest = &rest[end + separator..];
    }

    out.push_str(&url_password(rest));
    out
}

/// One word with its password masked, when it is a URL that has one.
fn url_password(word: &str) -> String {
    let Some((scheme, rest)) = word.split_once("://") else {
        return word.to_owned();
    };

    // The user and password end at the last @ before the path.

    let authority = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority].rfind('@') else {
        return word.to_owned();
    };

    match rest[..at].split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:{MASK}{}", &rest[at..]),
        None => word.to_owned(),
    }
}

/// A configuration document, as TOML, with the values of its `env`
/// tables masked as a worker's environment is: a variable written with
/// its value in a release's table is exported with it.
pub fn document_env(document: &str) -> Result<String, String> {
    let mut table: toml::Table = document.parse().map_err(|error| format!("{error}"))?;
    env_tables(&mut table);
    toml::to_string(&table).map_err(|error| error.to_string())
}

/// Mask the variables of every `env` table below this one.
fn env_tables(table: &mut toml::Table) {
    for (key, value) in table.iter_mut() {
        match value {
            toml::Value::Table(variables) if key == "env" => {
                for (name, written) in variables.iter_mut() {
                    if let toml::Value::String(text) = written {
                        *text = self::value(name, text);
                    }
                }
            }
            toml::Value::Table(inner) => env_tables(inner),
            toml::Value::Array(items) => {
                for item in items {
                    if let toml::Value::Table(inner) = item {
                        env_tables(inner);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Mask every value of a TOML table whose key says it is a secret (see
/// [`secret_setting`]), and the passwords of the URLs in the others,
/// through every table and array below it.
pub fn toml_table(table: &mut toml::Table) {
    for (key, value) in table.iter_mut() {
        toml_value(key, value);
    }
}

fn toml_value(key: &str, value: &mut toml::Value) {
    match value {
        toml::Value::Table(table) => toml_table(table),
        toml::Value::Array(items) => items.iter_mut().for_each(|item| toml_value(key, item)),
        other if secret_setting(key) => *other = toml::Value::String(MASK.to_owned()),
        toml::Value::String(text) => *text = url_passwords(text),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_masked_by_its_name_and_a_url_by_its_password() {
        assert_eq!(value("API_TOKEN", "abc123"), "***");
        assert_eq!(value("sentry_dsn", "https://k@sentry.example/1"), "***");
        assert_eq!(
            value("DJANGO_SETTINGS_MODULE", "shop.settings"),
            "shop.settings"
        );
        assert_eq!(
            value("DATABASE_URL", "postgres://shop:hunter2@db.example/shop"),
            "postgres://shop:***@db.example/shop"
        );
        assert_eq!(
            value(
                "REDIS_URLS",
                "redis://:pw1@a:6379/0, redis://u:p@w2@b:6379/1;redis://c:6379"
            ),
            "redis://:***@a:6379/0, redis://u:***@b:6379/1;redis://c:6379"
        );
        assert_eq!(
            value("CALLBACK", "https://shop.example/cb?next=mailto:a@b"),
            "https://shop.example/cb?next=mailto:a@b"
        );
        assert_eq!(
            value("USER_URL", "https://alice@shop.example/"),
            "https://alice@shop.example/"
        );
    }

    #[test]
    fn the_variables_of_a_document_are_masked_as_an_environment_is() {
        let masked = document_env(
            r#"
            [app.shop.release.r3]
            source = "/srv/shop"

            [app.shop.release.r3.env]
            API_TOKEN = "abc123"
            DATABASE_URL = "postgres://shop:hunter2@db/shop"
            MODE = "production"
            STRIPE = { secret = "file/stripe" }

            [[route]]
            host = "shop"
            "#,
        )
        .unwrap();
        let table: toml::Table = masked.parse().unwrap();
        let env = &table["app"]["shop"]["release"]["r3"]["env"];

        assert_eq!(env["API_TOKEN"].as_str(), Some("***"));
        assert_eq!(
            env["DATABASE_URL"].as_str(),
            Some("postgres://shop:***@db/shop")
        );
        assert_eq!(env["MODE"].as_str(), Some("production"));
        assert_eq!(env["STRIPE"]["secret"].as_str(), Some("file/stripe"));
        assert_eq!(
            table["app"]["shop"]["release"]["r3"]["source"].as_str(),
            Some("/srv/shop")
        );
        assert!(document_env("not = [toml").is_err());
    }

    #[test]
    fn a_configuration_file_keeps_its_shape_and_loses_its_secrets() {
        let mut table: toml::Table = toml::from_str(
            r#"
            [daemon]
            debug_exec = true

            [dashboard]
            token = "s3cret"
            listen = "127.0.0.1:9000"

            [[listener]]
            address = "0.0.0.0:443"
            tls_key = "/etc/muru/key.pem"

            [events.webhook.ops]
            url = "https://ops:hunter2@hooks.example/muru"
            secret = "hmac"
            retries = 3
            signing_key = 7

            [rate_limit.api]
            key = ["client", "path"]
            hash_key = "client"
            keepalive = "5s"
            "#,
        )
        .unwrap();
        toml_table(&mut table);

        assert_eq!(
            toml::to_string(&table).unwrap(),
            toml::to_string(
                &toml::from_str::<toml::Table>(
                    r#"
                    [daemon]
                    debug_exec = true

                    [dashboard]
                    token = "***"
                    listen = "127.0.0.1:9000"

                    [[listener]]
                    address = "0.0.0.0:443"
                    tls_key = "***"

                    [events.webhook.ops]
                    url = "https://ops:***@hooks.example/muru"
                    secret = "***"
                    retries = 3
                    signing_key = "***"

                    [rate_limit.api]
                    key = ["client", "path"]
                    hash_key = "client"
                    keepalive = "5s"
                    "#
                )
                .unwrap()
            )
            .unwrap()
        );
    }
}
