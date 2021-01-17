use crate::account::AccountNum;
use crate::spv::CrossValidationResult;
use crate::Error;
use aes_gcm_siv::aead::{generic_array::GenericArray, AeadInPlace, NewAead};
use aes_gcm_siv::Aes256GcmSiv;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::util::bip32::{DerivationPath, ExtendedPubKey};
use bitcoin::{BlockHash, Script, Transaction, Txid};
use elements::OutPoint;
use gdk_common::be::Unblinded;
use gdk_common::be::{BEBlockHeader, BETransaction, BETransactions};
use gdk_common::model::{FeeEstimate, SPVVerifyResult, Settings};
use gdk_common::NetworkId;
use log::{info, warn};
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub const BATCH_SIZE: u32 = 20;

pub type Store = Arc<RwLock<StoreMeta>>;

/// RawCache is a persisted and encrypted cache of wallet data, contains stuff like wallet transactions
/// It is fully reconstructable from xpub and data from electrum server (plus master blinding for elements)
#[derive(Default, Serialize, Deserialize)]
pub struct RawCache {
    /// account-specific information (transactions, scripts, history, indexes, unblinded)
    pub accounts: HashMap<AccountNum, RawAccountCache>,

    /// contains headers at the height of my txs (used to show tx timestamps)
    pub headers: HashMap<u32, BEBlockHeader>,

    /// verification status of Txid (could be only Verified or NotVerified, absence means InProgress)
    pub txs_verif: HashMap<Txid, SPVVerifyResult>,

    /// cached fee_estimates
    pub fee_estimates: Vec<FeeEstimate>,

    /// height and hash of tip of the blockchain
    pub tip: (u32, BlockHash),

    /// registry assets last modified, used when making the http request
    pub assets_last_modified: String,

    /// registry icons last modified, used when making the http request
    pub icons_last_modified: String,

    /// the result of the last spv cross-validation execution
    pub cross_validation_result: Option<CrossValidationResult>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct RawAccountCache {
    /// contains all my tx and all prevouts
    pub all_txs: BETransactions,

    /// contains all my script up to an empty batch of BATCHSIZE
    pub paths: HashMap<Script, DerivationPath>,

    /// inverse of `paths`
    pub scripts: HashMap<DerivationPath, Script>,

    /// contains only my wallet txs with the relative heights (None if unconfirmed)
    pub heights: HashMap<Txid, Option<u32>>,

    /// unblinded values (only for liquid)
    pub unblinded: HashMap<OutPoint, Unblinded>,

    /// max used indexes for external derivation /0/* and internal derivation /1/* (change)
    pub indexes: Indexes,
}

/// RawStore contains data that are not extractable from xpub+blockchain
/// like wallet settings and memos
#[derive(Default, Serialize, Deserialize)]
pub struct RawStore {
    /// wallet settings
    settings: Option<Settings>,

    /// transaction memos
    memos: HashMap<AccountNum, HashMap<Txid, String>>,
}

pub struct StoreMeta {
    pub cache: RawCache,
    pub store: RawStore,
    id: NetworkId,
    path: PathBuf,
    cipher: Aes256GcmSiv,
}

impl Drop for StoreMeta {
    fn drop(&mut self) {
        self.flush().unwrap();
    }
}

#[derive(Debug, PartialEq, Eq, Default, Clone, Serialize, Deserialize)]
pub struct Indexes {
    pub external: u32, // m/0/*
    pub internal: u32, // m/1/*
}

impl RawCache {
    /// create a new RawCache, loading data from a file if any and if there is no error in reading
    /// errors such as corrupted file or model change in the db, result in a empty store that will be repopulated
    fn new<P: AsRef<Path>>(path: P, cipher: &Aes256GcmSiv) -> Self {
        Self::try_new(path, cipher).unwrap_or_else(|e| {
            warn!("Initialize cache as default {:?}", e);
            Default::default()
        })
    }

    fn try_new<P: AsRef<Path>>(path: P, cipher: &Aes256GcmSiv) -> Result<Self, Error> {
        let decrypted = load_decrypt("cache", path, cipher)?;
        let store = serde_cbor::from_slice(&decrypted)?;
        Ok(store)
    }
}

impl RawStore {
    /// create a new RawStore, loading data from a file if any and if there is no error in reading
    /// errors such as corrupted file or model change in the db, result in a empty store that will be repopulated
    fn new<P: AsRef<Path>>(path: P, cipher: &Aes256GcmSiv) -> Self {
        Self::try_new(path, cipher).unwrap_or_else(|e| {
            warn!("Initialize store as default {:?}", e);
            Default::default()
        })
    }

    fn try_new<P: AsRef<Path>>(path: P, cipher: &Aes256GcmSiv) -> Result<Self, Error> {
        let decrypted = load_decrypt("store", path, cipher)?;
        let store = serde_cbor::from_slice(&decrypted)?;
        Ok(store)
    }
}

fn load_decrypt<P: AsRef<Path>>(
    name: &str,
    path: P,
    cipher: &Aes256GcmSiv,
) -> Result<Vec<u8>, Error> {
    let now = Instant::now();
    let mut store_path = PathBuf::from(path.as_ref());
    store_path.push(name);
    if !store_path.exists() {
        return Err(Error::Generic(format!("{:?} do not exist", store_path)));
    }
    let mut file = File::open(&store_path)?;
    let mut nonce_bytes = [0u8; 12];
    file.read_exact(&mut nonce_bytes)?;
    let nonce = GenericArray::from_slice(&nonce_bytes);
    let mut ciphertext = vec![];
    file.read_to_end(&mut ciphertext)?;

    cipher.decrypt_in_place(nonce, b"", &mut ciphertext)?;
    let plaintext = ciphertext;

    info!("loading {:?} took {}ms", &store_path, now.elapsed().as_millis());
    Ok(plaintext)
}

impl StoreMeta {
    pub fn new<P: AsRef<Path>>(
        path: P,
        xpub: ExtendedPubKey,
        id: NetworkId,
    ) -> Result<StoreMeta, Error> {
        let mut enc_key_data = vec![];
        enc_key_data.extend(&xpub.public_key.to_bytes());
        enc_key_data.extend(&xpub.chain_code.to_bytes());
        enc_key_data.extend(&xpub.network.magic().to_be_bytes());
        let key_bytes = sha256::Hash::hash(&enc_key_data).into_inner();
        let key = GenericArray::from_slice(&key_bytes);
        let cipher = Aes256GcmSiv::new(&key);
        let cache = RawCache::new(path.as_ref(), &cipher);
        let store = RawStore::new(path.as_ref(), &cipher);
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }

        Ok(StoreMeta {
            cache,
            store,
            id,
            cipher,
            path,
        })
    }

    fn flush_serializable<T: serde::Serialize>(&self, name: &str, value: &T) -> Result<(), Error> {
        let now = Instant::now();
        let mut nonce_bytes = [0u8; 12];
        thread_rng().fill(&mut nonce_bytes);
        let nonce = GenericArray::from_slice(&nonce_bytes);
        let mut plaintext = serde_cbor::to_vec(value)?;

        self.cipher.encrypt_in_place(nonce, b"", &mut plaintext)?;
        let ciphertext = plaintext;

        let mut store_path = self.path.clone();
        store_path.push(name);
        //TODO should avoid rewriting if not changed? it involves saving plaintext (or struct hash)
        // in the front of the file
        let mut file = File::create(&store_path)?;
        file.write(&nonce_bytes)?;
        file.write(&ciphertext)?;
        info!(
            "flushing {} bytes on {:?} took {}ms",
            ciphertext.len() + 16,
            &store_path,
            now.elapsed().as_millis()
        );
        Ok(())
    }

    fn flush_store(&self) -> Result<(), Error> {
        self.flush_serializable("store", &self.store)?;
        Ok(())
    }

    fn flush_cache(&self) -> Result<(), Error> {
        self.flush_serializable("cache", &self.cache)?;
        Ok(())
    }

    pub fn flush(&self) -> Result<(), Error> {
        self.flush_store()?;
        self.flush_cache()?;
        Ok(())
    }

    fn read(&self, name: &str) -> Result<Option<Value>, Error> {
        let mut path = self.path.clone();
        path.push(name);
        if path.exists() {
            let mut file = File::open(path)?;
            let mut buffer = vec![];
            info!("start read from {}", name);
            file.read_to_end(&mut buffer)?;
            info!("end read from {}, start parsing json", name);
            let value = serde_json::from_slice(&buffer)?;
            info!("end parsing json {}", name);
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    fn write(&self, name: &str, value: &Value) -> Result<(), Error> {
        let mut path = self.path.clone();
        path.push(name);
        let mut file = File::create(path)?;
        let vec = serde_json::to_vec(value)?;
        info!("start write {} bytes to {}", vec.len(), name);
        file.write(&vec)?;
        info!("end write {} bytes to {}", vec.len(), name);
        Ok(())
    }

    pub fn account_store(&self, account_num: AccountNum) -> Result<&RawAccountCache, Error> {
        self.cache
            .accounts
            .get(&account_num)
            .ok_or_else(|| Error::InvalidSubaccount(account_num.into()))
    }

    pub fn account_store_mut(
        &mut self,
        account_num: AccountNum,
    ) -> Result<&mut RawAccountCache, Error> {
        self.cache
            .accounts
            .get_mut(&account_num)
            .ok_or_else(|| Error::InvalidSubaccount(account_num.into()))
    }

    pub fn account_nums(&self) -> HashSet<AccountNum> {
        self.cache.accounts.keys().copied().collect()
    }

    pub fn read_asset_icons(&self) -> Result<Option<Value>, Error> {
        self.read("asset_icons")
    }

    /// write asset icons to a local file
    /// it is stored out of the encrypted area since it's public info
    pub fn write_asset_icons(&self, asset_icons: &Value) -> Result<(), Error> {
        self.write("asset_icons", asset_icons)
    }

    pub fn read_asset_registry(&self) -> Result<Option<Value>, Error> {
        self.read("asset_registry")
    }

    /// write asset registry to a local file
    /// it is stored out of the encrypted area since it's public info
    pub fn write_asset_registry(&self, asset_registry: &Value) -> Result<(), Error> {
        self.write("asset_registry", asset_registry)
    }

    pub fn fee_estimates(&self) -> Vec<FeeEstimate> {
        if self.cache.fee_estimates.is_empty() {
            let min_fee = match self.id {
                NetworkId::Bitcoin(_) => 1000,
                NetworkId::Elements(_) => 100,
            };
            vec![FeeEstimate(min_fee); 25]
        } else {
            self.cache.fee_estimates.clone()
        }
    }

    pub fn insert_memo(
        &mut self,
        account_num: AccountNum,
        txid: Txid,
        memo: &str,
    ) -> Result<(), Error> {
        self.store.memos.entry(account_num).or_default().insert(txid, memo.to_string());
        self.flush_store()?;
        Ok(())
    }

    pub fn get_memo(&self, account_num: AccountNum, txid: &Txid) -> Option<&String> {
        self.store.memos.get(&account_num).and_then(|a| a.get(txid))
    }

    pub fn insert_settings(&mut self, settings: Option<Settings>) -> Result<(), Error> {
        self.store.settings = settings;
        self.flush_store()?;
        Ok(())
    }

    pub fn get_settings(&self) -> Option<Settings> {
        self.store.settings.clone()
    }

    pub fn spv_verification_status(&self, txid: &Txid) -> SPVVerifyResult {
        // @shesek TODO support mult account
        let acc_store = match self.account_store(0usize.into()) {
            Ok(store) => store,
            Err(_) => return SPVVerifyResult::NotVerified,
        };

        if let Some(height) = acc_store.heights.get(txid).unwrap_or(&None) {
            match &self.cache.cross_validation_result {
                Some(CrossValidationResult::Invalid(inv)) if *height > inv.common_ancestor => {
                    // Report an SPV validation failure if the transaction was confirmed after the forking point
                    SPVVerifyResult::NotLongest
                }
                _ => self.cache.txs_verif.get(txid).cloned().unwrap_or(SPVVerifyResult::InProgress),
            }
        } else {
            SPVVerifyResult::Unconfirmed
        }
    }

    pub fn export_cache(&self) -> Result<RawCache, Error> {
        self.flush_cache()?;
        RawCache::try_new(&self.path, &self.cipher)
    }
}

impl RawAccountCache {
    pub fn get_bitcoin_tx(&self, txid: &Txid) -> Result<Transaction, Error> {
        match self.all_txs.get(txid) {
            Some(BETransaction::Bitcoin(tx)) => Ok(tx.clone()),
            _ => Err(Error::Generic("expected bitcoin tx".to_string())),
        }
    }

    pub fn get_liquid_tx(&self, txid: &Txid) -> Result<elements::Transaction, Error> {
        match self.all_txs.get(txid) {
            Some(BETransaction::Elements(tx)) => Ok(tx.clone()),
            _ => Err(Error::Generic("expected liquid tx".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::store::StoreMeta;
    use bitcoin::hashes::hex::FromHex;
    use bitcoin::util::bip32::ExtendedPubKey;
    use bitcoin::{Network, Txid};
    use gdk_common::NetworkId;
    use std::str::FromStr;
    use tempdir::TempDir;

    #[test]
    fn test_db_roundtrip() {
        let mut dir = TempDir::new("unit_test").unwrap().into_path();
        dir.push("store");
        let xpub = ExtendedPubKey::from_str("tpubD6NzVbkrYhZ4YfG9CySHqKHFbaLcD7hSDyqRUtCmMKNim5fkiJtTnFeqKsRHMHSK5ddFrhqRr3Ghv1JtuWkBzikuBqKu1xCpjQ9YxoPGgqU").unwrap();
        let txid =
            Txid::from_hex("f4184fc596403b9d638783cf57adfe4c75c605f6356fbc91338530e9831e9e16")
                .unwrap();

        let id = NetworkId::Bitcoin(Network::Testnet);
        let mut store = StoreMeta::new(&dir, xpub, None, id).unwrap();
        store.cache.heights.insert(txid, Some(1));
        drop(store);

        let store = StoreMeta::new(&dir, xpub, None, id).unwrap();
        assert_eq!(store.cache.heights.get(&txid), Some(&Some(1)));
    }
}
