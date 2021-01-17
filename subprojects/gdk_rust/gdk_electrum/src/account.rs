use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt;
use std::str::FromStr;

use log::{debug, info, trace};
use rand::Rng;
use serde::{Deserialize, Serialize};

use bitcoin::hashes::{hex::FromHex, Hash};
use bitcoin::secp256k1::{self, Message};
use bitcoin::util::address::Payload;
use bitcoin::util::bip143::SigHashCache;
use bitcoin::util::bip32::{DerivationPath, ExtendedPrivKey, ExtendedPubKey};
use bitcoin::{Address, PublicKey, Script, SigHashType, Transaction, Txid};
use elements::confidential::Value;

use gdk_common::be::{
    BEAddress, BEOutPoint, BETransaction, ScriptBatch, UTXOInfo, Utxos, DUST_VALUE,
};
use gdk_common::error::fn_err;
use gdk_common::model::{
    AddressAmount, AddressPointer, Balances, CreateTransaction, GetTransactionsOpt,
    SPVVerifyResult, TransactionMeta,
};
use gdk_common::scripts::{p2pkh_script, p2shwpkh_script, p2shwpkh_script_sig};
use gdk_common::wally::{
    asset_blinding_key_to_ec_private_key, ec_public_key_from_private_key, MasterBlindingKey,
};
use gdk_common::{ElementsNetwork, Network, NetworkId};

use crate::error::Error;
use crate::store::{Store, BATCH_SIZE};

lazy_static! {
    static ref EC: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::new();
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountNum(pub u32);

pub struct Account {
    account_num: AccountNum,
    path: DerivationPath,
    xpub: ExtendedPubKey,
    xprv: ExtendedPrivKey,
    chains: [ExtendedPubKey; 2],
    network: Network,
    store: Store,
    // elements only
    master_blinding: Option<MasterBlindingKey>,
}

impl Account {
    pub fn new(
        network: Network,
        master_xprv: &ExtendedPrivKey,
        master_blinding: Option<MasterBlindingKey>,
        store: Store,
        account_num: AccountNum,
    ) -> Result<Self, Error> {
        let path = get_account_path(account_num, &network)?;

        debug!("Using derivation path {} for account {}", path, account_num);

        let xprv = master_xprv.derive_priv(&EC, &path)?;
        let xpub = ExtendedPubKey::from_private(&EC, &xprv);

        // cache internal/external chains
        let chains = [xpub.ckd_pub(&EC, 0.into())?, xpub.ckd_pub(&EC, 1.into())?];

        Ok(Self {
            network,
            account_num,
            path,
            xpub,
            xprv,
            chains,
            store,
            master_blinding,
        })
    }

    pub fn num(&self) -> AccountNum {
        self.account_num
    }

    pub fn derive_address(&self, is_change: bool, index: u32) -> Result<BEAddress, Error> {
        let chain_xpub = self.chains[is_change as usize];
        let derived = chain_xpub.ckd_pub(&EC, index.into())?;

        match self.network.id() {
            NetworkId::Bitcoin(network) => {
                Ok(BEAddress::Bitcoin(Address::p2shwpkh(&derived.public_key, network).unwrap()))
            }
            NetworkId::Elements(network) => {
                let master_blinding_key = self
                    .master_blinding
                    .as_ref()
                    .expect("we are in elements but master blinding is None");

                let address = elements_address(&derived.public_key, master_blinding_key, network);
                Ok(BEAddress::Elements(address))
            }
        }
    }

    pub fn get_next_address(&self) -> Result<AddressPointer, Error> {
        let pointer = {
            let store = &mut self.store.write()?;
            let acc_store = store.account_store_mut(self.account_num)?;
            acc_store.indexes.external += 1;
            acc_store.indexes.external
        };
        let address = self.derive_address(false, pointer)?.to_string();
        Ok(AddressPointer {
            address,
            pointer,
        })
    }

    pub fn list_tx(&self, opt: &GetTransactionsOpt) -> Result<Vec<TransactionMeta>, Error> {
        let store = self.store.read()?;
        let acc_store = store.account_store(self.account_num)?;

        let mut txs = vec![];
        let mut my_txids: Vec<(&Txid, &Option<u32>)> = acc_store.heights.iter().collect();
        my_txids.sort_by(|a, b| {
            let height_cmp = b.1.unwrap_or(std::u32::MAX).cmp(&a.1.unwrap_or(std::u32::MAX));
            match height_cmp {
                Ordering::Equal => b.0.cmp(a.0),
                h @ _ => h,
            }
        });

        for (tx_id, height) in my_txids.iter().skip(opt.first).take(opt.count) {
            trace!("tx_id {}", tx_id);

            let tx = acc_store
                .all_txs
                .get(*tx_id)
                .ok_or_else(fn_err(&format!("list_tx no tx {}", tx_id)))?;
            let header = height.map(|h| store.cache.headers.get(&h)).flatten();
            trace!("tx_id {} header {:?}", tx_id, header);
            let mut addressees = vec![];
            for i in 0..tx.output_len() as u32 {
                let script = tx.output_script(i);
                if !script.is_empty() && !acc_store.paths.contains_key(&script) {
                    let address = tx.output_address(i, self.network.id());
                    trace!("tx_id {}:{} not my script, address {:?}", tx_id, i, address);
                    addressees.push(AddressAmount {
                        address: address.unwrap_or_else(|| "".to_string()),
                        satoshi: 0, // apparently not needed in list_tx addressees
                        asset_tag: None,
                    });
                }
            }
            let memo = store.get_memo(self.account_num, tx_id).cloned();

            let create_transaction = CreateTransaction {
                addressees,
                memo,
                ..Default::default()
            };

            let fee = tx.fee(
                &acc_store.all_txs,
                &acc_store.unblinded,
                &self.network.policy_asset().ok(),
            )?;
            trace!("tx_id {} fee {}", tx_id, fee);

            let satoshi =
                tx.my_balance_changes(&acc_store.all_txs, &acc_store.paths, &acc_store.unblinded);
            trace!("tx_id {} balances {:?}", tx_id, satoshi);

            // We define an incoming txs if there are more assets received by the wallet than spent
            // when they are equal it's an outgoing tx because the special asset liquid BTC
            // is negative due to the fee being paid
            // TODO how do we label issuance tx?
            let negatives = satoshi.iter().filter(|(_, v)| **v < 0).count();
            let positives = satoshi.iter().filter(|(_, v)| **v > 0).count();
            let (type_, user_signed) = match (
                positives > negatives,
                tx.is_redeposit(&acc_store.paths, &acc_store.all_txs),
            ) {
                (_, true) => ("redeposit", true),
                (true, false) => ("incoming", false),
                (false, false) => ("outgoing", true),
            };

            let spv_verified = if self.network.spv_enabled.unwrap_or(false) {
                store.spv_verification_status(tx_id)
            } else {
                SPVVerifyResult::Disabled
            };

            trace!(
                "tx_id {} type {} user_signed {} spv_verified {:?}",
                tx_id,
                type_,
                user_signed,
                spv_verified
            );

            let tx_meta = TransactionMeta::new(
                tx.clone(),
                **height,
                header.map(|h| h.time()),
                satoshi,
                fee,
                self.network.id().get_bitcoin_network().unwrap_or(bitcoin::Network::Bitcoin),
                type_.to_string(),
                create_transaction,
                user_signed,
                spv_verified,
            );

            txs.push(tx_meta);
        }
        info!("list_tx {:?}", txs.iter().map(|e| &e.txid).collect::<Vec<&String>>());

        Ok(txs)
    }

    pub fn utxos(&self) -> Result<Utxos, Error> {
        info!("start utxos");
        let store_read = self.store.read()?;
        let acc_store = store_read.account_store(self.account_num)?;

        let mut utxos = vec![];
        let spent = self.spent()?;
        for (tx_id, height) in acc_store.heights.iter() {
            let tx = acc_store
                .all_txs
                .get(tx_id)
                .ok_or_else(fn_err(&format!("utxos no tx {}", tx_id)))?;
            let tx_utxos: Vec<(BEOutPoint, UTXOInfo)> = match tx {
                BETransaction::Bitcoin(tx) => tx
                    .output
                    .clone()
                    .into_iter()
                    .enumerate()
                    .filter(|(_, output)| output.value > DUST_VALUE)
                    .map(|(vout, output)| (BEOutPoint::new_bitcoin(tx.txid(), vout as u32), output))
                    .filter_map(|(vout, output)| {
                        acc_store.paths.get(&output.script_pubkey).map(|path| (vout, output, path))
                    })
                    .filter(|(outpoint, _, _)| !spent.contains(&outpoint))
                    .map(|(outpoint, output, path)| {
                        (
                            outpoint,
                            UTXOInfo::new(
                                "btc".to_string(),
                                output.value,
                                output.script_pubkey,
                                height.clone(),
                                path.clone(),
                            ),
                        )
                    })
                    .collect(),
                BETransaction::Elements(tx) => {
                    let policy_asset = self.network.policy_asset_id()?;
                    tx.output
                        .clone()
                        .into_iter()
                        .enumerate()
                        .map(|(vout, output)| {
                            (BEOutPoint::new_elements(tx.txid(), vout as u32), output)
                        })
                        .filter_map(|(vout, output)| {
                            acc_store
                                .paths
                                .get(&output.script_pubkey)
                                .map(|path| (vout, output, path))
                        })
                        .filter(|(outpoint, _, _)| !spent.contains(&outpoint))
                        .filter_map(|(outpoint, output, path)| {
                            if let BEOutPoint::Elements(el_outpoint) = outpoint {
                                if let Some(unblinded) = acc_store.unblinded.get(&el_outpoint) {
                                    if unblinded.value < DUST_VALUE
                                        && unblinded.asset == policy_asset
                                    {
                                        return None;
                                    }
                                    return Some((
                                        outpoint,
                                        UTXOInfo::new(
                                            unblinded.asset_hex(),
                                            unblinded.value,
                                            output.script_pubkey,
                                            height.clone(),
                                            path.clone(),
                                        ),
                                    ));
                                }
                            }
                            None
                        })
                        .collect()
                }
            };
            utxos.extend(tx_utxos);
        }
        utxos.sort_by(|a, b| (b.1).value.cmp(&(a.1).value));

        Ok(utxos)
    }

    fn spent(&self) -> Result<HashSet<BEOutPoint>, Error> {
        let store_read = self.store.read()?;
        let acc_store = store_read.account_store(self.account_num)?;
        let mut result = HashSet::new();
        for tx in acc_store.all_txs.values() {
            let outpoints: Vec<BEOutPoint> = match tx {
                BETransaction::Bitcoin(tx) => {
                    tx.input.iter().map(|i| BEOutPoint::Bitcoin(i.previous_output)).collect()
                }
                BETransaction::Elements(tx) => {
                    tx.input.iter().map(|i| BEOutPoint::Elements(i.previous_output)).collect()
                }
            };
            result.extend(outpoints.into_iter());
        }
        Ok(result)
    }

    pub fn balance(&self) -> Result<Balances, Error> {
        info!("start balance");
        let mut result = HashMap::new();
        match self.network.id() {
            NetworkId::Bitcoin(_) => result.entry("btc".to_string()).or_insert(0),
            NetworkId::Elements(_) => {
                result.entry(self.network.policy_asset.as_ref().unwrap().clone()).or_insert(0)
            }
        };
        for (_, info) in self.utxos()?.iter() {
            *result.entry(info.asset.clone()).or_default() += info.value as i64;
        }
        Ok(result)
    }

    pub fn create_tx(&self, request: &mut CreateTransaction) -> Result<TransactionMeta, Error> {
        create_tx(self, request)
    }

    // TODO when we can serialize psbt
    //pub fn sign(&self, psbt: PartiallySignedTransaction) -> Result<PartiallySignedTransaction, Error> { Err(Error::Generic("NotImplemented".to_string())) }
    pub fn sign(&self, request: &TransactionMeta) -> Result<TransactionMeta, Error> {
        info!("sign");
        let be_tx = BETransaction::deserialize(&hex::decode(&request.hex)?, self.network.id())?;
        let store_read = self.store.read()?;
        let acc_store = store_read.account_store(self.account_num)?;

        let mut betx: TransactionMeta = match be_tx {
            BETransaction::Bitcoin(tx) => {
                let mut out_tx = tx.clone();

                for i in 0..tx.input.len() {
                    let prev_output = tx.input[i].previous_output;
                    info!("input#{} prev_output:{:?}", i, prev_output);
                    let prev_tx = acc_store.get_bitcoin_tx(&prev_output.txid)?;
                    let out = prev_tx.output[prev_output.vout as usize].clone();
                    let derivation_path: DerivationPath = acc_store
                        .paths
                        .get(&out.script_pubkey)
                        .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                        .clone();
                    info!(
                        "input#{} prev_output:{:?} derivation_path:{:?}",
                        i, prev_output, derivation_path
                    );

                    let (script_sig, witness) =
                        internal_sign_bitcoin(&tx, i, &self.xprv, &derivation_path, out.value);

                    out_tx.input[i].script_sig = script_sig;
                    out_tx.input[i].witness = witness;
                }
                let tx = BETransaction::Bitcoin(out_tx);
                info!(
                    "transaction final size is {} bytes and {} vbytes",
                    tx.serialize().len(),
                    tx.get_weight() / 4
                );
                info!("FINALTX inputs:{} outputs:{}", tx.input_len(), tx.output_len());
                tx.into()
            }
            BETransaction::Elements(mut tx) => {
                blind_tx(self, &mut tx)?;

                for i in 0..tx.input.len() {
                    let prev_output = tx.input[i].previous_output;
                    info!("input#{} prev_output:{:?}", i, prev_output);
                    let prev_tx = acc_store.get_liquid_tx(&prev_output.txid)?;
                    let out = prev_tx.output[prev_output.vout as usize].clone();
                    let derivation_path: DerivationPath = acc_store
                        .paths
                        .get(&out.script_pubkey)
                        .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                        .clone();

                    let (script_sig, witness) =
                        internal_sign_elements(&tx, i, &self.xprv, &derivation_path, out.value);

                    tx.input[i].script_sig = script_sig;
                    tx.input[i].witness.script_witness = witness;
                }

                let fee: u64 =
                    tx.output.iter().filter(|o| o.is_fee()).map(|o| o.minimum_value()).sum();
                let tx = BETransaction::Elements(tx);
                info!(
                    "transaction final size is {} bytes and {} vbytes and fee is {}",
                    tx.serialize().len(),
                    tx.get_weight() / 4,
                    fee
                );
                info!("FINALTX inputs:{} outputs:{}", tx.input_len(), tx.output_len());
                tx.into()
            }
        };

        betx.fee = request.fee;
        betx.create_transaction = request.create_transaction.clone();

        drop(acc_store);
        drop(store_read);
        let mut store_write = self.store.write()?;
        let mut acc_store = store_write.account_store_mut(self.account_num)?;

        let changes_used = request.changes_used.unwrap_or(0);
        if changes_used > 0 {
            info!("tx used {} changes", changes_used);
            // The next sync would update the internal index but we increment the internal index also
            // here after sign so that if we immediately create another tx we are not reusing addresses
            // This implies signing multiple times without broadcasting leads to gaps in the internal chain
            acc_store.indexes.internal += changes_used;
        }

        if let Some(memo) = request.create_transaction.as_ref().and_then(|c| c.memo.as_ref()) {
            store_write.insert_memo(self.account_num, Txid::from_hex(&betx.txid)?, memo)?;
        }

        Ok(betx)
    }

    pub fn get_script_batch(&self, is_change: bool, batch: u32) -> Result<ScriptBatch, Error> {
        let store = self.store.read()?;
        let acc_store = store.account_store(self.account_num)?;

        let mut result = ScriptBatch::default();
        result.cached = true;

        let start = batch * BATCH_SIZE;
        let end = start + BATCH_SIZE;
        for j in start..end {
            let path = DerivationPath::from(&[(is_change as u32).into(), j.into()][..]);
            let script = acc_store.scripts.get(&path).cloned().map_or_else(
                || -> Result<Script, Error> {
                    result.cached = false;
                    Ok(self.derive_address(is_change, j)?.script_pubkey())
                },
                Ok,
            )?;
            result.value.push((script, path));
        }
        Ok(result)
    }
}

impl fmt::Display for AccountNum {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl From<u32> for AccountNum {
    fn from(num: u32) -> Self {
        AccountNum(num)
    }
}
impl From<usize> for AccountNum {
    fn from(num: usize) -> Self {
        AccountNum(num as u32)
    }
}
impl Into<u32> for AccountNum {
    fn into(self) -> u32 {
        self.0
    }
}

impl AccountNum {
    pub fn as_u32(self) -> u32 {
        self.into()
    }
}

fn get_account_path(
    account_num: AccountNum,
    network: &Network,
) -> Result<DerivationPath, Error> {
    let coin_type = get_coin_type(network);
    let purpose = 49; // P2SH-P2WPKH
    // BIP44: m / purpose' / coin_type' / account' / change / address_index
    let path: DerivationPath =
        format!("m/{}'/{}'/{}'", purpose, coin_type, account_num).parse().unwrap();

    Ok(path)
}

fn get_coin_type(network: &Network) -> u32 {
    // coin_type = 0 bitcoin, 1 testnet, 1776 liquid bitcoin as defined in https://github.com/satoshilabs/slips/blob/master/slip-0044.md
    // slip44 suggest 1 for every testnet, so we are using it also for regtest
    match network.id() {
        NetworkId::Bitcoin(bitcoin_network) => match bitcoin_network {
            bitcoin::Network::Bitcoin => 0,
            bitcoin::Network::Testnet => 1,
            bitcoin::Network::Regtest => 1,
        },
        NetworkId::Elements(elements_network) => match elements_network {
            ElementsNetwork::Liquid => 1776,
            ElementsNetwork::ElementsRegtest => 1,
        },
    }
}

fn elements_address(
    public_key: &PublicKey,
    master_blinding_key: &MasterBlindingKey,
    net: ElementsNetwork,
) -> elements::Address {
    let script = p2shwpkh_script(public_key);
    let blinding_key = asset_blinding_key_to_ec_private_key(&master_blinding_key, &script);
    let blinding_pub = ec_public_key_from_private_key(blinding_key);

    let addr_params = elements_address_params(net);

    elements::Address::p2shwpkh(public_key, Some(blinding_pub), addr_params)
}

fn elements_address_params(net: ElementsNetwork) -> &'static elements::AddressParams {
    match net {
        ElementsNetwork::Liquid => &elements::AddressParams::LIQUID,
        ElementsNetwork::ElementsRegtest => &elements::AddressParams::ELEMENTS,
    }
}

fn random32() -> Vec<u8> {
    rand::thread_rng().gen::<[u8; 32]>().to_vec()
}

#[allow(clippy::cognitive_complexity)]
pub fn create_tx(
    account: &Account,
    request: &mut CreateTransaction,
) -> Result<TransactionMeta, Error> {
    info!("create_tx {:?}", request);
    let network = &account.network;

    // TODO put checks into CreateTransaction::validate, add check asset_tag are valid asset hex
    // eagerly check for address validity
    for address in request.addressees.iter().map(|a| &a.address) {
        match network.id() {
            NetworkId::Bitcoin(network) => {
                if let Ok(address) = bitcoin::Address::from_str(address) {
                    info!("address.network:{} network:{}", address.network, network);
                    if address.network == network
                        || (address.network == bitcoin::Network::Testnet
                            && network == bitcoin::Network::Regtest)
                    {
                        continue;
                    }
                    if let Payload::WitnessProgram {
                        version: v,
                        program: _p,
                    } = &address.payload
                    {
                        // Do not support segwit greater than v0
                        if v.to_u8() > 0 {
                            return Err(Error::InvalidAddress);
                        }
                    }
                }
                return Err(Error::InvalidAddress);
            }
            NetworkId::Elements(network) => {
                if let Ok(address) = elements::Address::from_str(address) {
                    info!(
                        "address.params:{:?} address_params(network):{:?}",
                        address.params,
                        elements_address_params(network)
                    );
                    if address.params == elements_address_params(network) {
                        continue;
                    }
                }
                return Err(Error::InvalidAddress);
            }
        }
    }

    if request.addressees.is_empty() {
        return Err(Error::EmptyAddressees);
    }

    let subaccount = request.subaccount.unwrap_or(0);
    if subaccount != 0 {
        return Err(Error::InvalidSubaccount(subaccount));
    }

    if !request.previous_transaction.is_empty() {
        return Err(Error::Generic("bump not supported".into()));
    }

    let send_all = request.send_all.unwrap_or(false);
    request.send_all = Some(send_all); // accept default false, but always return the value
    if !send_all && request.addressees.iter().any(|a| a.satoshi == 0) {
        return Err(Error::InvalidAmount);
    }

    if !send_all {
        for address_amount in request.addressees.iter() {
            if address_amount.satoshi <= DUST_VALUE {
                match network.id() {
                    NetworkId::Bitcoin(_) => return Err(Error::InvalidAmount),
                    NetworkId::Elements(_) => {
                        if address_amount.asset_tag == network.policy_asset {
                            // we apply dust rules for liquid bitcoin as elements do
                            return Err(Error::InvalidAmount);
                        }
                    }
                }
            }
        }
    }

    if let NetworkId::Elements(_) = network.id() {
        if request.addressees.iter().any(|a| a.asset_tag.is_none()) {
            return Err(Error::AssetEmpty);
        }
    }

    // convert from satoshi/kbyte to satoshi/byte
    let default_value = match network.id() {
        NetworkId::Bitcoin(_) => 1000,
        NetworkId::Elements(_) => 100,
    };
    let fee_rate = (request.fee_rate.unwrap_or(default_value) as f64) / 1000.0;
    info!("target fee_rate {:?} satoshi/byte", fee_rate);

    let utxos = match &request.utxos {
        None => account.utxos()?,
        Some(utxos) => utxos.try_into()?,
    };
    info!("utxos len:{} utxos:{:?}", utxos.len(), utxos);

    if send_all {
        // send_all works by creating a dummy tx with all utxos, estimate the fee and set the
        // sending amount to `total_amount_utxos - estimated_fee`
        info!("send_all calculating total_amount");
        if request.addressees.len() != 1 {
            return Err(Error::SendAll);
        }
        let asset = request.addressees[0].asset_tag.as_deref().unwrap_or("btc");
        let all_utxos: Vec<&(BEOutPoint, UTXOInfo)> =
            utxos.iter().filter(|(_, i)| i.asset == asset).collect();
        let total_amount_utxos: u64 = all_utxos.iter().map(|(_, i)| i.value).sum();

        let to_send = if asset == "btc" || Some(asset.to_string()) == network.policy_asset {
            let mut dummy_tx = BETransaction::new(network.id());
            for utxo in all_utxos.iter() {
                dummy_tx.add_input(utxo.0.clone());
            }
            let out = &request.addressees[0]; // safe because we checked we have exactly one recipient
            dummy_tx
                .add_output(&out.address, out.satoshi, out.asset_tag.clone())
                .map_err(|_| Error::InvalidAddress)?;
            let estimated_fee = dummy_tx.estimated_fee(fee_rate, 0) + 3; // estimating 3 satoshi more as estimating less would later result in InsufficientFunds
            total_amount_utxos.checked_sub(estimated_fee).ok_or_else(|| Error::InsufficientFunds)?
        } else {
            total_amount_utxos
        };

        info!("send_all asset: {} to_send:{}", asset, to_send);

        request.addressees[0].satoshi = to_send;
    }

    let mut tx = BETransaction::new(network.id());
    // transaction is created in 3 steps:
    // 1) adding requested outputs to tx outputs
    // 2) adding enough utxso to inputs such that tx outputs and estimated fees are covered
    // 3) adding change(s)

    // STEP 1) add the outputs requested for this transactions
    for out in request.addressees.iter() {
        tx.add_output(&out.address, out.satoshi, out.asset_tag.clone())
            .map_err(|_| Error::InvalidAddress)?;
    }

    // STEP 2) add utxos until tx outputs are covered (including fees) or fail
    let store_read = account.store.read()?;
    let acc_store = store_read.account_store(account.num())?;
    let mut used_utxo: HashSet<BEOutPoint> = HashSet::new();
    loop {
        let mut needs = tx.needs(
            fee_rate,
            send_all,
            network.policy_asset.clone(),
            &acc_store.all_txs,
            &acc_store.unblinded,
        ); // Vec<(asset_string, satoshi)  "policy asset" is last, in bitcoin asset_string="btc" and max 1 element
        info!("needs: {:?}", needs);
        if needs.is_empty() {
            // SUCCESS tx doesn't need other inputs
            break;
        }
        let current_need = needs.pop().unwrap(); // safe to unwrap just checked it's not empty

        // taking only utxos of current asset considered, filters also utxos used in this loop
        let mut asset_utxos: Vec<&(BEOutPoint, UTXOInfo)> = utxos
            .iter()
            .filter(|(o, i)| i.asset == current_need.asset && !used_utxo.contains(o))
            .collect();

        // sort by biggest utxo, random maybe another option, but it should be deterministically random (purely random breaks send_all algorithm)
        asset_utxos.sort_by(|a, b| (a.1).value.cmp(&(b.1).value));
        let utxo = asset_utxos.pop().ok_or(Error::InsufficientFunds)?;

        match network.id() {
            NetworkId::Bitcoin(_) => {
                // UTXO with same script must be spent together
                for other_utxo in utxos.iter() {
                    if (other_utxo.1).script == (utxo.1).script {
                        used_utxo.insert(other_utxo.0.clone());
                        tx.add_input(other_utxo.0.clone());
                    }
                }
            }
            NetworkId::Elements(_) => {
                // Don't spend same script together in liquid. This would allow an attacker
                // to cheaply send assets without value to the target, which will have to
                // waste fees for the extra tx inputs and (eventually) outputs.
                // While blinded address are required and not public knowledge,
                // they are still available to whom transacted with us in the past
                used_utxo.insert(utxo.0.clone());
                tx.add_input(utxo.0.clone());
            }
        }
    }

    // STEP 3) adding change(s)
    let estimated_fee = tx.estimated_fee(
        fee_rate,
        tx.estimated_changes(send_all, &acc_store.all_txs, &acc_store.unblinded),
    );
    let changes = tx.changes(
        estimated_fee,
        network.policy_asset.clone(),
        &acc_store.all_txs,
        &acc_store.unblinded,
    ); // Vec<Change> asset, value
    for (i, change) in changes.iter().enumerate() {
        let change_index = acc_store.indexes.internal + i as u32 + 1;
        let change_address = account.derive_address(true, change_index)?.to_string();
        info!(
            "adding change to {} of {} asset {:?}",
            &change_address, change.satoshi, change.asset
        );
        tx.add_output(&change_address, change.satoshi, Some(change.asset.clone()))?;
    }

    // randomize inputs and outputs, BIP69 has been rejected because lacks wallets adoption
    tx.scramble();

    let policy_asset = network.policy_asset().ok();
    let fee_val = tx.fee(&acc_store.all_txs, &acc_store.unblinded, &policy_asset)?; // recompute exact fee_val from built tx
    tx.add_fee_if_elements(fee_val, &policy_asset)?;

    info!("created tx fee {:?}", fee_val);

    let mut satoshi =
        tx.my_balance_changes(&acc_store.all_txs, &acc_store.paths, &acc_store.unblinded);

    for (_, v) in satoshi.iter_mut() {
        *v = v.abs();
    }

    let mut created_tx = TransactionMeta::new(
        tx,
        None,
        None,
        satoshi,
        fee_val,
        network.id().get_bitcoin_network().unwrap_or(bitcoin::Network::Bitcoin),
        "outgoing".to_string(),
        request.clone(),
        true,
        SPVVerifyResult::InProgress,
    );
    created_tx.changes_used = Some(changes.len() as u32);
    info!("returning: {:?}", created_tx);

    Ok(created_tx)
}

fn internal_sign_bitcoin(
    tx: &Transaction,
    input_index: usize,
    xprv: &ExtendedPrivKey,
    path: &DerivationPath,
    value: u64,
) -> (Script, Vec<Vec<u8>>) {
    let xprv = xprv.derive_priv(&EC, &path).unwrap();
    let private_key = &xprv.private_key;
    let public_key = &PublicKey::from_private_key(&EC, private_key);
    let witness_script = p2pkh_script(public_key);

    let hash =
        SigHashCache::new(tx).signature_hash(input_index, &witness_script, value, SigHashType::All);

    let message = Message::from_slice(&hash.into_inner()[..]).unwrap();
    let signature = EC.sign(&message, &private_key.key);

    let mut signature = signature.serialize_der().to_vec();
    signature.push(SigHashType::All as u8);

    let script_sig = p2shwpkh_script_sig(public_key);
    let witness = vec![signature, public_key.to_bytes()];
    info!(
        "added size len: script_sig:{} witness:{}",
        script_sig.len(),
        witness.iter().map(|v| v.len()).sum::<usize>()
    );

    (script_sig, witness)
}

fn internal_sign_elements(
    tx: &elements::Transaction,
    input_index: usize,
    xprv: &ExtendedPrivKey,
    path: &DerivationPath,
    value: Value,
) -> (Script, Vec<Vec<u8>>) {
    use gdk_common::wally::tx_get_elements_signature_hash;

    let xprv = xprv.derive_priv(&EC, &path).unwrap();
    let private_key = &xprv.private_key;
    let public_key = &PublicKey::from_private_key(&EC, private_key);

    let script_code = p2pkh_script(public_key);
    let sighash = tx_get_elements_signature_hash(
        &tx,
        input_index,
        &script_code,
        &value,
        SigHashType::All.as_u32(),
        true, // segwit
    );
    let message = secp256k1::Message::from_slice(&sighash[..]).unwrap();
    let signature = EC.sign(&message, &private_key.key);
    let mut signature = signature.serialize_der().to_vec();
    signature.push(SigHashType::All as u8);

    let script_sig = p2shwpkh_script_sig(public_key);
    let witness = vec![signature, public_key.to_bytes()];
    info!(
        "added size len: script_sig:{} witness:{}",
        script_sig.len(),
        witness.iter().map(|v| v.len()).sum::<usize>()
    );
    (script_sig, witness)
}

fn blind_tx(account: &Account, tx: &mut elements::Transaction) -> Result<(), Error> {
    use elements::confidential::{Asset, Nonce};
    use gdk_common::wally::{
        asset_final_vbf, asset_generator_from_bytes, asset_rangeproof, asset_surjectionproof,
        asset_value_commitment,
    };

    info!("blind_tx {}", tx.txid());

    let store_read = account.store.read()?;
    let acc_store = store_read.account_store(account.num())?;

    let mut input_assets = vec![];
    let mut input_abfs = vec![];
    let mut input_vbfs = vec![];
    let mut input_ags = vec![];
    let mut input_values = vec![];

    for input in tx.input.iter() {
        info!("input {:?}", input);

        let unblinded = acc_store
            .unblinded
            .get(&input.previous_output)
            .ok_or_else(|| Error::Generic("cannot find unblinded values".into()))?;
        info!("unblinded value: {} asset:{}", unblinded.value, hex::encode(&unblinded.asset[..]));

        input_values.push(unblinded.value);
        input_assets.extend(unblinded.asset.to_vec());
        input_abfs.extend(unblinded.abf.to_vec());
        input_vbfs.extend(unblinded.vbf.to_vec());
        let input_asset = asset_generator_from_bytes(&unblinded.asset, &unblinded.abf);
        input_ags.extend(elements::encode::serialize(&input_asset));
    }

    let ct_exp = account.network.ct_exponent.expect("ct_exponent not set in network");
    let ct_bits = account.network.ct_bits.expect("ct_bits not set in network");
    info!("ct params ct_exp:{}, ct_bits:{}", ct_exp, ct_bits);

    let mut output_blinded_values = vec![];
    for output in tx.output.iter() {
        if !output.is_fee() {
            output_blinded_values.push(output.minimum_value());
        }
    }
    info!("output_blinded_values {:?}", output_blinded_values);
    let mut all_values = vec![];
    all_values.extend(input_values);
    all_values.extend(output_blinded_values);
    let in_num = tx.input.len();
    let out_num = tx.output.len();

    let output_abfs: Vec<Vec<u8>> = (0..out_num - 1).map(|_| random32()).collect();
    let mut output_vbfs: Vec<Vec<u8>> = (0..out_num - 2).map(|_| random32()).collect();

    let mut all_abfs = vec![];
    all_abfs.extend(input_abfs.to_vec());
    all_abfs.extend(output_abfs.iter().cloned().flatten().collect::<Vec<u8>>());

    let mut all_vbfs = vec![];
    all_vbfs.extend(input_vbfs.to_vec());
    all_vbfs.extend(output_vbfs.iter().cloned().flatten().collect::<Vec<u8>>());

    let last_vbf = asset_final_vbf(all_values, in_num as u32, all_abfs, all_vbfs);
    output_vbfs.push(last_vbf.to_vec());

    for (i, mut output) in tx.output.iter_mut().enumerate() {
        info!("output {:?}", output);
        if !output.is_fee() {
            match (output.value, output.asset, output.nonce) {
                (Value::Explicit(value), Asset::Explicit(asset), Nonce::Confidential(_, _)) => {
                    info!("value: {}", value);
                    let nonce = elements::encode::serialize(&output.nonce);
                    let blinding_pubkey = PublicKey::from_slice(&nonce).unwrap();
                    let blinding_key = asset_blinding_key_to_ec_private_key(
                        account.master_blinding.as_ref().unwrap(),
                        &output.script_pubkey,
                    );
                    let blinding_public_key = ec_public_key_from_private_key(blinding_key);
                    let mut output_abf = [0u8; 32];
                    output_abf.copy_from_slice(&(&output_abfs[i])[..]);
                    let mut output_vbf = [0u8; 32];
                    output_vbf.copy_from_slice(&(&output_vbfs[i])[..]);
                    let asset = asset.clone().into_inner();

                    let output_generator =
                        asset_generator_from_bytes(&asset.into_inner(), &output_abf);
                    let output_value_commitment =
                        asset_value_commitment(value, output_vbf, output_generator);
                    let min_value = if output.script_pubkey.is_provably_unspendable() {
                        0
                    } else {
                        1
                    };

                    let rangeproof = asset_rangeproof(
                        value,
                        blinding_pubkey.key,
                        blinding_key,
                        asset.into_inner(),
                        output_abf,
                        output_vbf,
                        output_value_commitment,
                        &output.script_pubkey,
                        output_generator,
                        min_value,
                        ct_exp,
                        ct_bits,
                    );
                    trace!("asset: {}", hex::encode(&asset));
                    trace!("output_abf: {}", hex::encode(&output_abf));
                    trace!(
                        "output_generator: {}",
                        hex::encode(&elements::encode::serialize(&output_generator))
                    );
                    trace!("input_assets: {}", hex::encode(&input_assets));
                    trace!("input_abfs: {}", hex::encode(&input_abfs));
                    trace!("input_ags: {}", hex::encode(&input_ags));
                    trace!("in_num: {}", in_num);

                    let surjectionproof = asset_surjectionproof(
                        asset.into_inner(),
                        output_abf,
                        output_generator,
                        output_abf,
                        &input_assets,
                        &input_abfs,
                        &input_ags,
                        in_num,
                    );
                    trace!("surjectionproof: {}", hex::encode(&surjectionproof));

                    let bytes = blinding_public_key.serialize();
                    let byte32: [u8; 32] = bytes[1..].as_ref().try_into().unwrap();
                    output.nonce = elements::confidential::Nonce::Confidential(bytes[0], byte32);
                    output.asset = output_generator;
                    output.value = output_value_commitment;
                    info!(
                        "added size len: surjectionproof:{} rangeproof:{}",
                        surjectionproof.len(),
                        rangeproof.len()
                    );
                    output.witness.surjection_proof = surjectionproof;
                    output.witness.rangeproof = rangeproof;
                }
                _ => panic!("create_tx created things not right"),
            }
        }
    }
    Ok(())
}
