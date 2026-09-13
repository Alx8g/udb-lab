//! Loopback-only PostgreSQL adapter. Server instance is started by the campaign
//! owner, never an application connection string. Databases are new per trial.
use super::*;
use postgres::{Client, IsolationLevel, NoTls, Statement};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Pg {
    client: Option<Client>,
    port: u16,
    database: String,
    statements: Vec<Statement>,
    version: String,
    settings: Value,
    path: PathBuf,
}
impl Pg {
    fn connect(port: u16, database: &str) -> Result<Client> {
        // Port alone does not establish ownership. Every connection verifies the
        // generated cluster name and canonical server data directory before writes.
        let marker_path = std::env::var_os("SPI_PG_CLUSTER_FILE")
            .ok_or("SPI_PG_CLUSTER_FILE is required for the isolated server")?;
        let marker_path = PathBuf::from(marker_path).canonicalize()?;
        let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".working/tmp")
            .canonicalize()?;
        if !marker_path.starts_with(&scratch) {
            return Err(
                "PostgreSQL identity file must be in this project's scratch directory".into(),
            );
        }
        let marker: Value = serde_json::from_slice(&fs::read(&marker_path)?)?;
        if marker["schema"] != json!(1)
            || marker["host"] != json!("127.0.0.1")
            || marker["port"].as_u64() != Some(port as u64)
        {
            return Err("invalid isolated PostgreSQL identity".into());
        }
        let name = marker["cluster_name"]
            .as_str()
            .ok_or("cluster marker missing")?;
        if !name.starts_with("spi_benchmark_") || name.len() != 46 {
            return Err("invalid PostgreSQL cluster marker".into());
        }
        let expected_dir = PathBuf::from(
            marker["data_directory"]
                .as_str()
                .ok_or("cluster directory missing")?,
        )
        .canonicalize()?;
        if expected_dir
            != marker_path
                .parent()
                .ok_or("cluster identity parent missing")?
                .join("cluster")
                .canonicalize()?
        {
            return Err("PostgreSQL identity does not own its data directory".into());
        }
        let password =
            std::env::var("SPI_PG_PASSWORD").map_err(|_| "isolated PostgreSQL password missing")?;
        let mut client = postgres::Config::new()
            .host("127.0.0.1")
            .port(port)
            .connect_timeout(std::time::Duration::from_secs(10))
            .user("spi_bench")
            .password(password)
            .dbname(database)
            .connect(NoTls)?;
        let row = client.query_one(
            "SELECT current_setting('cluster_name'), current_setting('data_directory')",
            &[],
        )?;
        let observed_name: String = row.get(0);
        let observed_dir: String = row.get(1);
        if observed_name != name || PathBuf::from(observed_dir).canonicalize()? != expected_dir {
            return Err("refusing unowned PostgreSQL server".into());
        }
        Ok(client)
    }
    pub fn new(path: PathBuf) -> Result<Self> {
        fs::create_dir(&path)?;
        let port: u16 = std::env::var("SPI_PG_PORT")
            .map_err(|_| "SPI_PG_PORT must name the isolated campaign server")?
            .parse()?;
        let mut admin = Self::connect(port, "postgres")?;
        let version: String = admin.query_one("SHOW server_version", &[])?.get(0);
        for setting in ["fsync", "full_page_writes"] {
            let v: String = admin.query_one(&format!("SHOW {setting}"), &[])?.get(0);
            if v != "on" {
                return Err(format!("PostgreSQL {setting} must be on").into());
            }
        }
        let database = format!(
            "spi_bench_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        // Only digits and a literal prefix form this identifier. Never interpolate user input.
        admin.batch_execute(&format!("CREATE DATABASE {database}"))?;
        let mut client = Self::connect(port, &database)?;
        client.batch_execute("SET synchronous_commit=on; SET default_transaction_isolation='serializable'; CREATE TABLE kv(k BYTEA PRIMARY KEY, v BYTEA NOT NULL)")?;
        let mut settings = json!({});
        for setting in [
            "shared_buffers",
            "work_mem",
            "max_connections",
            "synchronous_commit",
            "default_transaction_isolation",
            "fsync",
            "full_page_writes",
        ] {
            settings[setting] = json!(client
                .query_one(&format!("SHOW {setting}"), &[])?
                .get::<_, String>(0));
        }
        let mut pg = Self {
            client: Some(client),
            port,
            database,
            statements: vec![],
            version,
            settings,
            path,
        };
        pg.prepare()?;
        fs::write(
            pg.path.join("server-database.json"),
            serde_json::to_vec_pretty(
                &json!({"host":"127.0.0.1","port":port,"database":pg.database}),
            )?,
        )?;
        Ok(pg)
    }
    fn prepare(&mut self) -> Result<()> {
        self.statements.clear();
        let client = self.client.as_mut().unwrap();
        client.batch_execute(
            "SET synchronous_commit=on; SET default_transaction_isolation='serializable'",
        )?;
        for sql in [
            "INSERT INTO kv VALUES($1,$2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            "SELECT v FROM kv WHERE k=$1",
            "SELECT k,v FROM kv WHERE k>=$1 AND k<$2 ORDER BY k",
            "SELECT k,v FROM kv WHERE k>=$1 ORDER BY k",
        ] {
            self.statements.push(client.prepare(sql)?);
        }
        Ok(())
    }
}
impl Engine for Pg {
    fn batch(&mut self, rows: &[Entry]) -> Result<()> {
        let mut tx = self
            .client
            .as_mut()
            .unwrap()
            .build_transaction()
            .isolation_level(IsolationLevel::Serializable)
            .start()?;
        for r in rows {
            tx.execute(&self.statements[0], &[&r.key, &r.value])?;
        }
        tx.commit()?;
        Ok(())
    }
    fn get(&mut self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .client
            .as_mut()
            .unwrap()
            .query_opt(&self.statements[1], &[&k])?
            .map(|r| r.get(0)))
    }
    fn range(&mut self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Entry>> {
        let rs = if let Some(end) = end {
            self.client
                .as_mut()
                .unwrap()
                .query(&self.statements[2], &[&start, &end])?
        } else {
            self.client
                .as_mut()
                .unwrap()
                .query(&self.statements[3], &[&start])?
        };
        Ok(rs
            .into_iter()
            .map(|r| Entry {
                key: r.get(0),
                value: r.get(1),
            })
            .collect())
    }
    fn clear(&mut self) -> Result<()> {
        Ok(())
    } // Shared server/OS cache cannot be safely purged per query.
    fn reopen(&mut self) -> Result<()> {
        self.client.take();
        self.client = Some(Self::connect(self.port, &self.database)?);
        self.prepare()
    }
    fn maintain(&mut self) -> Result<()> {
        // VACUUM cannot be grouped with CHECKPOINT in an implicit transaction.
        self.client
            .as_mut()
            .unwrap()
            .batch_execute("VACUUM (ANALYZE) kv")?;
        self.client.as_mut().unwrap().batch_execute("CHECKPOINT")?;
        Ok(())
    }
    fn stats(&self) -> Result<Value> {
        // Stats are outside timed calls and use an independent connection.
        let mut c = Self::connect(self.port, &self.database)?;
        let bytes: i64 = c
            .query_one("SELECT pg_total_relation_size('kv')", &[])?
            .get(0);
        Ok(
            json!({"physical_bytes":bytes,"physical_scope":"table+index+TOAST relation size, excludes cluster/WAL/shared catalog",
            "server_version":self.version,"settings":self.settings,"transport":"loopback TCP, prepared SQL",
            "cache_clear":"not supported; shared_buffers/OS remain warm","reopen":"client reconnect, NOT server restart or crash recovery",
            "maintenance":"VACUUM ANALYZE plus CHECKPOINT, not full compaction",
            "contract":"new per-trial database on isolated local server, durable serializable single-writer transactions; no multi-client throughput tested"}),
        )
    }
}
