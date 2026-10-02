use std::{collections::HashMap, path::Path};

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::{ActionId, ItemId, ProviderId};

pub struct HistoryStore {
    connection: std::sync::Mutex<Connection>,
}

#[derive(Clone, Debug)]
pub struct UsageEvent {
    pub query: String,
    pub provider: ProviderId,
    pub item: ItemId,
    pub action: ActionId,
    pub timestamp: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageStats {
    pub count: u64,
    pub last_used: Option<i64>,
}

impl HistoryStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }
    fn from_connection(connection: Connection) -> Result<Self> {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS usage_events (provider TEXT NOT NULL, item TEXT NOT NULL, query TEXT NOT NULL, action TEXT NOT NULL, timestamp INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS usage_item ON usage_events(provider, item);")?;
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
        })
    }
    pub fn record(&self, event: &UsageEvent) -> Result<()> {
        self.connection.lock().map_err(|_| anyhow::anyhow!("history lock poisoned"))?.execute(
            "INSERT INTO usage_events(provider,item,query,action,timestamp) VALUES (?1,?2,?3,?4,?5)",
            params![event.provider.0, event.item.0, event.query, event.action.0, event.timestamp],
        )?;
        Ok(())
    }
    pub(crate) fn snapshot_for(
        &self,
        items: &[crate::Item],
    ) -> Result<HashMap<(String, String), UsageStats>> {
        if items.is_empty() {
            return Ok(HashMap::new());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("history lock poisoned"))?;
        let mut unique = std::collections::HashSet::new();
        let keys: Vec<_> = items
            .iter()
            .filter_map(|item| {
                let key = (item.provider.0.clone(), item.id.0.clone());
                unique.insert(key.clone()).then_some(key)
            })
            .collect();
        let pairs = vec!["(?, ?)"; keys.len()].join(", ");
        let sql = format!(
            "SELECT provider, item, COUNT(*), MAX(timestamp) FROM usage_events WHERE (provider, item) IN ({pairs}) GROUP BY provider, item"
        );
        let params: Vec<&str> = keys
            .iter()
            .flat_map(|(provider, item)| [provider.as_str(), item.as_str()])
            .collect();
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                UsageStats {
                    count: row.get::<_, i64>(2)?.max(0) as u64,
                    last_used: row.get(3)?,
                },
            ))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(Into::into)
    }

    pub fn stats(&self, provider: &ProviderId, item: &ItemId) -> Result<UsageStats> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("history lock poisoned"))?;
        let (count, last_used): (i64, Option<i64>) = connection.query_row(
            "SELECT COUNT(*), MAX(timestamp) FROM usage_events WHERE provider=?1 AND item=?2",
            params![provider.0, item.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(UsageStats {
            count: count.max(0) as u64,
            last_used,
        })
    }
}
