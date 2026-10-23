//! Each source with each container destination, through the real CLI: an independent download,
//! a native restore, and the same schema and data. Run the tests with
//! `scripts/test-integration.sh`. Each test prints one evidence line with the versions.

mod common;

use common::{
    cli::{self, MYSQL_SECRETS, POSTGRESQL_SECRETS, Setup},
    floci, hex, mysql, postgresql, ssh,
};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
enum Destination {
    S3,
    Sftp,
    Scp,
}

impl Destination {
    fn name(self) -> &'static str {
        match self {
            Destination::S3 => "s3",
            Destination::Sftp => "sftp",
            Destination::Scp => "scp",
        }
    }

    /// The configuration section, and the bucket or directory that receives the files.
    async fn prepare(self, test: &str) -> (String, String) {
        match self {
            Destination::S3 => {
                let bucket = floci::bucket(&test.replace('_', "-"), None).await;
                (cli::floci(&bucket), bucket)
            }
            Destination::Sftp | Destination::Scp => {
                let dir = ssh::directory("/srv/backups", test);
                let host = ssh::var("SVALBARD_TEST_SSH_HOST");
                (
                    cli::ssh_destination(self.name(), &host, 22, &dir, "client"),
                    dir,
                )
            }
        }
    }

    /// Downloads a file without Svalbard: curl for Floci, the shell of `backup` for SSH.
    fn download(self, location: &str, key: &str) -> Vec<u8> {
        match self {
            Destination::S3 => floci::download(location, key),
            Destination::Sftp | Destination::Scp => ssh::download(&format!("{location}/{key}")),
        }
    }

    /// Everything at the destination, sorted, including unfinished uploads and hidden files.
    async fn contents(self, location: &str) -> Vec<String> {
        match self {
            Destination::S3 => {
                let mut contents = floci::objects(location).await;
                contents.extend(floci::uploads(location).await);
                contents.sort();
                contents
            }
            Destination::Sftp | Destination::Scp => ssh::files(location),
        }
    }

    /// The server version, for the evidence line.
    fn version(self) -> String {
        match self {
            Destination::S3 => format!("Floci {}", ssh::var("SVALBARD_TEST_FLOCI_VERSION")),
            Destination::Sftp | Destination::Scp => ssh::shell("/usr/sbin/sshd -V 2>&1 || true")
                .trim()
                .to_owned(),
        }
    }

    /// Checks the stored files against the CLI result, and returns the dump.
    async fn check(self, location: &str, result: &serde_json::Value) -> Vec<u8> {
        assert_eq!(result["result"], "complete", "{result}");
        let (data, record) = (str(&result["data"]), str(&result["record"]));
        // Only the dump and its record: no unfinished upload and no temporary file.
        let key = |name: &str| name.rsplit('/').next().unwrap().to_owned();
        let mut expected = match self {
            Destination::S3 => vec![data.to_owned(), record.to_owned()],
            Destination::Sftp | Destination::Scp => vec![key(data), key(record)],
        };
        expected.sort();
        assert_eq!(self.contents(location).await, expected);

        let dump = self.download(location, data);
        assert_eq!(result["size"], dump.len());
        assert_eq!(result["sha256"], hex(&Sha256::digest(&dump)));
        let record: serde_json::Value =
            serde_json::from_slice(&self.download(location, record)).unwrap();
        assert_eq!(record["sha256"], result["sha256"]);
        assert_eq!(record["size"], result["size"]);
        assert_eq!(record["data"], result["data"]);
        dump
    }

    /// Prints the evidence line.
    fn report(self, source: &str, result: &serde_json::Value) {
        println!(
            "matrix: {source} {} ({}) -> {} {}: restored {} bytes",
            str(&result["server_version"]),
            str(&result["client_version"]),
            self.name(),
            self.version(),
            result["size"]
        );
    }
}

fn str(value: &serde_json::Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{value} is not a string"))
}

async fn postgresql(destination: Destination) {
    let test = format!("matrix_pg_{}", destination.name());
    let mut fixture = postgresql::Fixture::create(&test).await;
    fixture.add_larger().await;
    let (config, location) = destination.prepare(&test).await;
    let setup = Setup::new(&test, &cli::postgresql_source(&fixture), &config, "");
    let outcome = setup.run(postgresql::PASSWORD, POSTGRESQL_SECRETS).await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dump = destination.check(&location, &outcome.result).await;
    fixture.assert_restores(&dump).await;
    destination.report("postgresql", &outcome.result);
}

async fn mysql(destination: Destination) {
    let test = format!("matrix_my_{}", destination.name());
    let mut fixture = mysql::Fixture::create(&test).await;
    fixture.add_larger().await;
    let (config, location) = destination.prepare(&test).await;
    let setup = Setup::new(&test, &cli::mysql_source(&fixture), &config, "");
    let outcome = setup.run(mysql::PASSWORD, MYSQL_SECRETS).await;

    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    let dump = destination.check(&location, &outcome.result).await;
    fixture.assert_restores(&dump).await;
    destination.report("mysql", &outcome.result);
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn postgresql_to_s3() {
    postgresql(Destination::S3).await;
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn postgresql_to_sftp() {
    postgresql(Destination::Sftp).await;
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn postgresql_to_scp() {
    postgresql(Destination::Scp).await;
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn mysql_to_s3() {
    mysql(Destination::S3).await;
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn mysql_to_sftp() {
    mysql(Destination::Sftp).await;
}

#[tokio::test]
#[ignore = "needs Docker: run scripts/test-integration.sh"]
async fn mysql_to_scp() {
    mysql(Destination::Scp).await;
}
