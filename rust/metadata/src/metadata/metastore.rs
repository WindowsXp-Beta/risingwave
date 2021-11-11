use crate::metadata::Epoch;
use async_trait::async_trait;
use risingwave_common::array::RwError;
use risingwave_common::error::ErrorCode::{InternalError, ItemNotFound};
use risingwave_common::error::Result;
use rocksdb::{Direction, IteratorMode, Options, WriteBatch, DB, DEFAULT_COLUMN_FAMILY_NAME};
use std::collections::{BTreeMap, HashMap};
use std::fmt::{Display, Formatter};
use std::str;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::task;

pub const DEFAULT_COLUMN_FAMILY: &str = DEFAULT_COLUMN_FAMILY_NAME;

/// `MetaStore` defines the functions used to operate metadata.
#[async_trait]
pub trait MetaStore: Sync + Send {
    async fn list(&self) -> Result<Vec<Vec<u8>>>;
    async fn put(&self, key: &[u8], value: &[u8], version: Epoch) -> Result<()>;
    async fn put_batch(&self, tuples: Vec<(&[u8], &[u8], Epoch)>) -> Result<()>;
    async fn get(&self, key: &[u8], version: Epoch) -> Result<Vec<u8>>;
    async fn delete(&self, key: &[u8], version: Epoch) -> Result<()>;
    async fn delete_all(&self, key: &[u8]) -> Result<()>;

    async fn list_cf(&self, cf: &str) -> Result<Vec<Vec<u8>>>;
    async fn put_cf(&self, cf: &str, key: &[u8], value: &[u8], version: Epoch) -> Result<()>;
    async fn put_batch_cf(&self, tuples: Vec<(&str, &[u8], &[u8], Epoch)>) -> Result<()>;
    async fn get_cf(&self, cf: &str, key: &[u8], version: Epoch) -> Result<Vec<u8>>;
    async fn delete_cf(&self, cf: &str, key: &[u8], version: Epoch) -> Result<()>;
    async fn delete_all_cf(&self, cf: &str, key: &[u8]) -> Result<()>;
}

pub type MetaStoreRef = Box<dyn MetaStore>;

/// `RocksdbStore` stores key value pairs into rocksdb,
/// the generate key would be format as: `id`-`epoch`.
/// different types of `Entity` should be placed in different cf.
// TODO: add more options for db and cfs check.
pub struct RocksdbStore {
    db: Arc<DB>,
}

impl RocksdbStore {
    pub fn new(path: &str, cfs: Vec<&str>) -> Self {
        let mut db_opts = Options::default();
        db_opts.create_missing_column_families(true);
        db_opts.create_if_missing(true);
        let db = DB::open_cf(&db_opts, path, cfs).unwrap();
        RocksdbStore { db: Arc::new(db) }
    }
}

#[derive(Clone)]
struct KeyForRocksdb(Vec<u8>);

impl KeyForRocksdb {
    pub fn compose(key: &[u8], version: Epoch) -> KeyForRocksdb {
        KeyForRocksdb(
            format!(
                "{}-{:020}",
                str::from_utf8(key).unwrap(),
                !version.into_inner()
            )
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    pub fn key(&self) -> Vec<u8> {
        let key_str = str::from_utf8(self.0.as_slice()).unwrap();
        let idx = key_str.rfind('-').unwrap();
        key_str[..idx].as_bytes().to_vec()
    }

    pub fn version(&self) -> Epoch {
        let key_str = str::from_utf8(self.0.as_slice()).unwrap().to_string();
        let idx = key_str.rfind('-').unwrap();
        Epoch::from(!u64::from_str(&key_str[idx + 1..]).unwrap())
    }

    pub fn next_key(&self) -> Vec<u8> {
        let mut key = self.key();
        let len = key.len();
        key[len - 1] += 1;
        key
    }
}

impl Display for KeyForRocksdb {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", str::from_utf8(&self.0).unwrap())
    }
}

#[async_trait]
impl MetaStore for RocksdbStore {
    async fn list(&self) -> Result<Vec<Vec<u8>>> {
        let db = self.db.clone();

        task::spawn_blocking(move || {
            let mut result = vec![];
            let mut iter = db.iterator(IteratorMode::Start);
            loop {
                let val = iter.next();
                if val.is_none() {
                    break;
                }
                let (k, v) = val.unwrap();
                result.push(v.to_vec());

                iter.set_mode(IteratorMode::From(
                    &KeyForRocksdb(k.to_vec()).next_key(),
                    Direction::Forward,
                ))
            }
            result
        })
        .await
        .map_err(|e| InternalError(e.to_string()).into())
    }

    async fn put(&self, key: &[u8], value: &[u8], version: Epoch) -> Result<()> {
        let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
        let db = self.db.clone();
        let value_owned = value.to_owned();

        task::spawn_blocking(move || {
            db.put(wrap_key, value_owned)
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }

    async fn put_batch(&self, tuples: Vec<(&[u8], &[u8], Epoch)>) -> Result<()> {
        let mut batch = WriteBatch::default();
        for (key, value, version) in tuples {
            let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
            batch.put(wrap_key, value);
        }
        let db = self.db.clone();

        task::spawn_blocking(move || {
            db.write(batch)
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }

    async fn get(&self, key: &[u8], version: Epoch) -> Result<Vec<u8>> {
        let wrap_key = KeyForRocksdb::compose(key, version);
        let db = self.db.clone();

        task::spawn_blocking(move || {
            db.get(wrap_key.into_inner())
                .unwrap()
                .ok_or_else(|| RwError::from(ItemNotFound("entry not found".to_string())))
        })
        .await?
    }

    async fn delete(&self, key: &[u8], version: Epoch) -> Result<()> {
        let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
        let db = self.db.clone();

        task::spawn_blocking(move || {
            db.delete(wrap_key)
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }

    async fn delete_all(&self, key: &[u8]) -> Result<()> {
        self.delete_all_cf(DEFAULT_COLUMN_FAMILY, key).await
    }

    async fn list_cf(&self, cf: &str) -> Result<Vec<Vec<u8>>> {
        let db = self.db.clone();
        let cfs = cf.to_owned();

        task::spawn_blocking(move || {
            let handle = db.cf_handle(cfs.as_str()).unwrap();
            let mut result = vec![];
            let mut iter = db.iterator_cf(handle, IteratorMode::Start);
            loop {
                let val = iter.next();
                if val.is_none() {
                    break;
                }
                let (k, v) = val.unwrap();
                result.push(v.to_vec());

                iter.set_mode(IteratorMode::From(
                    &KeyForRocksdb(k.to_vec()).next_key(),
                    Direction::Forward,
                ))
            }
            result
        })
        .await
        .map_err(|e| InternalError(e.to_string()).into())
    }

    async fn put_cf(&self, cf: &str, key: &[u8], value: &[u8], version: Epoch) -> Result<()> {
        let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
        let db = self.db.clone();
        let cfs = cf.to_owned();
        let value_owned = value.to_owned();

        task::spawn_blocking(move || {
            let handle = db.cf_handle(cfs.as_str()).unwrap();
            db.put_cf(handle, wrap_key, value_owned)
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }

    async fn put_batch_cf(&self, tuples: Vec<(&str, &[u8], &[u8], Epoch)>) -> Result<()> {
        let mut batch = WriteBatch::default();
        let db = self.db.clone();
        for (cf, key, value, version) in tuples {
            let handle = db.cf_handle(cf).unwrap();
            let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
            batch.put_cf(handle, wrap_key, value);
        }

        task::spawn_blocking(move || {
            db.write(batch)
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }

    async fn get_cf(&self, cf: &str, key: &[u8], version: Epoch) -> Result<Vec<u8>> {
        let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
        let db = self.db.clone();
        let cfs = cf.to_owned();

        task::spawn_blocking(move || {
            let handle = db.cf_handle(cfs.as_str()).unwrap();
            db.get_cf(handle, wrap_key)
                .unwrap()
                .ok_or_else(|| RwError::from(ItemNotFound("entry not found".to_string())))
        })
        .await?
    }

    async fn delete_cf(&self, cf: &str, key: &[u8], version: Epoch) -> Result<()> {
        let wrap_key = KeyForRocksdb::compose(key, version).into_inner();
        let db = self.db.clone();
        let cfs = cf.to_owned();

        task::spawn_blocking(move || {
            let handle = db.cf_handle(cfs.as_str()).unwrap();
            db.delete_cf(handle, wrap_key)
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }

    async fn delete_all_cf(&self, cf: &str, key: &[u8]) -> Result<()> {
        // FIXME: for those keys already contains `-` inside, this will not work.
        let start = format!("{}-", str::from_utf8(key).unwrap());
        let end = format!("{}-a", str::from_utf8(key).unwrap());
        let db = self.db.clone();
        let cfs = cf.to_owned();

        task::spawn_blocking(move || {
            let handle = db.cf_handle(cfs.as_str()).unwrap();
            db.delete_range_cf(handle, start.as_bytes(), end.as_bytes())
                .map_err(|e| InternalError(e.to_string()).into())
        })
        .await?
    }
}

type KeyForMem = (Vec<u8>, String);

pub struct MemStore {
    entities: Mutex<HashMap<KeyForMem, BTreeMap<Epoch, Vec<u8>>>>,
}

impl MemStore {
    pub fn new() -> Self {
        MemStore {
            entities: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl MetaStore for MemStore {
    async fn list(&self) -> Result<Vec<Vec<u8>>> {
        self.list_cf(DEFAULT_COLUMN_FAMILY).await
    }

    async fn put(&self, key: &[u8], value: &[u8], version: Epoch) -> Result<()> {
        self.put_cf(DEFAULT_COLUMN_FAMILY, key, value, version)
            .await
    }

    async fn put_batch(&self, tuples: Vec<(&[u8], &[u8], Epoch)>) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        for (key, value, version) in tuples {
            match entities.get_mut(&(key.to_vec(), String::from(DEFAULT_COLUMN_FAMILY))) {
                Some(entry) => {
                    entry.insert(version, value.to_vec());
                }
                None => {
                    let mut entry = BTreeMap::new();
                    entry.insert(version, value.to_vec());
                    entities.insert((key.to_vec(), String::from(DEFAULT_COLUMN_FAMILY)), entry);
                }
            }
        }
        Ok(())
    }

    async fn get(&self, key: &[u8], version: Epoch) -> Result<Vec<u8>> {
        self.get_cf(DEFAULT_COLUMN_FAMILY, key, version).await
    }

    async fn delete(&self, key: &[u8], version: Epoch) -> Result<()> {
        self.delete_cf(DEFAULT_COLUMN_FAMILY, key, version).await
    }

    async fn delete_all(&self, key: &[u8]) -> Result<()> {
        self.delete_all_cf(DEFAULT_COLUMN_FAMILY, key).await
    }

    async fn list_cf(&self, cf: &str) -> Result<Vec<Vec<u8>>> {
        let entities = self.entities.lock().unwrap();
        Ok(entities
            .iter()
            .filter(|(k, v)| k.1 == cf && !v.is_empty())
            .map(|(_, v)| v.iter().last().unwrap().1.clone())
            .collect::<Vec<_>>())
    }

    async fn put_cf(&self, cf: &str, key: &[u8], value: &[u8], version: Epoch) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        match entities.get_mut(&(key.to_vec(), String::from(cf))) {
            Some(entry) => {
                entry.insert(version, value.to_vec());
            }
            None => {
                let mut entry = BTreeMap::new();
                entry.insert(version, value.to_vec());
                entities.insert((key.to_vec(), String::from(cf)), entry);
            }
        }

        Ok(())
    }

    async fn put_batch_cf(&self, tuples: Vec<(&str, &[u8], &[u8], Epoch)>) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        for (cf, key, value, version) in tuples {
            match entities.get_mut(&(key.to_vec(), String::from(cf))) {
                Some(entry) => {
                    entry.insert(version, value.to_vec());
                }
                None => {
                    let mut entry = BTreeMap::new();
                    entry.insert(version, value.to_vec());
                    entities.insert((key.to_vec(), String::from(cf)), entry);
                }
            }
        }
        Ok(())
    }

    async fn get_cf(&self, cf: &str, key: &[u8], version: Epoch) -> Result<Vec<u8>> {
        let entities = self.entities.lock().unwrap();
        entities
            .get(&(key.to_vec(), String::from(cf)))
            .ok_or_else(|| RwError::from(ItemNotFound("entry not found".to_string())))?
            .get(&version)
            .cloned()
            .ok_or_else(|| ItemNotFound("entry not found".to_string()).into())
    }

    async fn delete_cf(&self, cf: &str, key: &[u8], version: Epoch) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        entities
            .get_mut(&(key.to_vec(), String::from(cf)))
            .and_then(|entry| entry.remove(&version));

        Ok(())
    }

    async fn delete_all_cf(&self, cf: &str, key: &[u8]) -> Result<()> {
        let mut entities = self.entities.lock().unwrap();
        entities.remove(&(key.to_vec(), String::from(cf)));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_metadata_store() -> Result<()> {
        let dir = tempdir().unwrap();
        let test_path = dir.path().join("temporary_rocksdb_path");
        let rocksdb_store = RocksdbStore::new(test_path.to_str().unwrap(), vec!["test_cf"]);
        let mem_store = MemStore::new();

        let stores: Vec<MetaStoreRef> = vec![Box::new(rocksdb_store), Box::new(mem_store)];
        for store in stores {
            let (k, v, version) = (b"key_1", b"value_1", Epoch::from(1));
            assert!(store.put(k, v, version).await.is_ok());
            let val = store.get(k, version).await.unwrap();
            assert_eq!(val.as_slice(), v);
            let val = store
                .get_cf(DEFAULT_COLUMN_FAMILY, k, version)
                .await
                .unwrap();
            assert_eq!(val.as_slice(), v);
            assert!(store.get_cf("test_cf", k, version).await.is_err());

            assert!(store.put_cf("test_cf", k, v, version).await.is_ok());
            let val = store.get_cf("test_cf", k, version).await.unwrap();
            assert_eq!(val.as_slice(), v);

            assert!(store.delete(k, version).await.is_ok());
            assert_eq!(store.list().await.unwrap().len(), 0);

            assert!(store.delete_cf("test_cf", k, version).await.is_ok());
            assert_eq!(store.list_cf("test_cf").await.unwrap().len(), 0);

            let batch_values: Vec<(&[u8], &[u8], Epoch)> = vec![
                (b"key_1", b"value_1", Epoch::from(2)),
                (b"key_2", b"value_2", Epoch::from(2)),
                (b"key_3", b"value_3", Epoch::from(2)),
            ];
            assert!(store.put_batch(batch_values).await.is_ok());

            let batch_values: Vec<(&str, &[u8], &[u8], Epoch)> = vec![
                ("test_cf", b"key_1", b"value_1", Epoch::from(2)),
                ("test_cf", b"key_2", b"value_2", Epoch::from(2)),
            ];
            assert!(store.put_batch_cf(batch_values).await.is_ok());

            assert_eq!(store.list().await.unwrap().len(), 3);
            assert_eq!(store.list_cf("test_cf").await.unwrap().len(), 2);

            assert!(store
                .put(b"key_3", b"value_3_new", Epoch::from(3))
                .await
                .is_ok());
            let mut values = store.list().await.unwrap();
            values.sort();
            let expected: Vec<Vec<u8>> = vec![
                b"value_1".to_vec(),
                b"value_2".to_vec(),
                b"value_3_new".to_vec(),
            ];
            assert_eq!(values, expected);

            assert!(store.delete_all(b"key_1").await.is_ok());
            assert!(store.delete_all(b"key_2").await.is_ok());
            assert!(store.delete_all(b"key_3").await.is_ok());
            assert_eq!(store.list().await.unwrap().len(), 0);

            assert!(store.delete_all_cf("test_cf", b"key_1").await.is_ok());
            assert!(store.delete_all_cf("test_cf", b"key_2").await.is_ok());
            assert_eq!(store.list_cf("test_cf").await.unwrap().len(), 0);
        }

        Ok(())
    }

    #[test]
    fn test_key_for_rocksdb() -> Result<()> {
        let key1 = KeyForRocksdb::compose(b"key-1", Epoch::from(1));
        let key2 = KeyForRocksdb::compose(b"key-2", Epoch::from(2));
        assert_eq!(
            str::from_utf8(&key1.clone().into_inner()).unwrap(),
            "key-1-18446744073709551614"
        );
        assert_eq!(
            str::from_utf8(&key2.clone().into_inner()).unwrap(),
            "key-2-18446744073709551613"
        );
        assert_eq!(key1.key(), b"key-1".as_slice().to_vec());
        assert_eq!(key2.key(), b"key-2".as_slice().to_vec());
        assert_eq!(key1.version().into_inner(), 1);
        assert_eq!(key2.version().into_inner(), 2);
        assert_eq!(key1.next_key(), b"key-2".as_slice().to_vec());
        assert_eq!(key2.next_key(), b"key-3".as_slice().to_vec());

        Ok(())
    }
}
