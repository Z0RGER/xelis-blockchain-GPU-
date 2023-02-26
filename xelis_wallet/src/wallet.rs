use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Error, Context};
use tokio::sync::{Mutex, RwLock};
use xelis_common::api::DataType;
use xelis_common::config::XELIS_ASSET;
use xelis_common::crypto::address::Address;
use xelis_common::crypto::hash::Hash;
use xelis_common::crypto::key::{KeyPair, PublicKey};
use xelis_common::network::Network;
use xelis_common::serializer::{Serializer, Writer};
use xelis_common::transaction::{TransactionType, Transfer, Transaction, EXTRA_DATA_LIMIT_SIZE};
use crate::cipher::Cipher;
use crate::config::{PASSWORD_ALGORITHM, PASSWORD_HASH_SIZE, SALT_SIZE};
use crate::mnemonics;
use crate::network_handler::{NetworkHandler, SharedNetworkHandler};
use crate::storage::{EncryptedStorage, Storage};
use crate::transaction_builder::TransactionBuilder;
use chacha20poly1305::{aead::OsRng, Error as CryptoError};
use rand::RngCore;
use thiserror::Error;
use log::{error, debug};

#[derive(Error, Debug)]
pub enum WalletError {
    #[error("Invalid key pair")]
    InvalidKeyPair,
    #[error("Expected a TX")]
    ExpectedOneTx,
    #[error("Transaction owner is the receiver")]
    TxOwnerIsReceiver,
    #[error("Error from crypto: {}", _0)]
    CryptoError(CryptoError),
    #[error("Unexpected error on database: {}", _0)]
    DatabaseError(#[from] sled::Error),
    #[error("Invalid encrypted value: minimum 25 bytes")]
    InvalidEncryptedValue,
    #[error("No salt found in storage")]
    NoSalt,
    #[error("Error while hashing: {}", _0)]
    AlgorithmHashingError(String),
    #[error("Error while fetching encrypted master key from DB")]
    NoMasterKeyFound,
    #[error("Invalid salt size stored in storage, expected 32 bytes")]
    InvalidSaltSize,
    #[error("Error while fetching password salt from DB")]
    NoSaltFound,
    #[error("Your wallet contains only {} instead of {} for asset {}", _0, _1, _2)]
    NotEnoughFunds(u64, u64, Hash),
    #[error("Your wallet don't have enough funds to pay fees: expected {} but have only {}", _0, _1)]
    NotEnoughFundsForFee(u64, u64),
    #[error("Invalid address params")]
    InvalidAddressParams,
    #[error("Invalid extra data in this transaction, expected maximum {} bytes but got {} bytes", _0, _1)]
    ExtraDataTooBig(usize, usize),
    #[error("Wallet is not in online mode")]
    NotOnlineMode,
    #[error("Wallet is already in online mode")]
    AlreadyOnlineMode,
    #[error("Asset is already present on disk")]
    AssetAlreadyRegistered,
    #[error("Topoheight is too high to rescan")]
    RescanTopoheightTooHigh,
    #[error(transparent)]
    Any(#[from] Error)
}

pub struct Wallet {
    // Encrypted Wallet Storage
    storage: RwLock<EncryptedStorage>,
    // Private & Public key linked for this wallet
    keypair: KeyPair,
    // network handler for online mode to keep wallet synced
    network_handler: Mutex<Option<SharedNetworkHandler>>,
    network: Network
}

pub fn hash_password(password: String, salt: &[u8]) -> Result<[u8; PASSWORD_HASH_SIZE], WalletError> {
    let mut output = [0; PASSWORD_HASH_SIZE];
    PASSWORD_ALGORITHM.hash_password_into(password.as_bytes(), salt, &mut output).map_err(|e| WalletError::AlgorithmHashingError(e.to_string()))?;
    Ok(output)
}

impl Wallet {
    fn new(storage: EncryptedStorage, keypair: KeyPair, network: Network) -> Arc<Self> {
        let zelf = Self {
            storage: RwLock::new(storage),
            keypair,
            network_handler: Mutex::new(None),
            network
        };

        Arc::new(zelf)
    }

    pub fn create(name: String, password: String, seed: Option<String>, network: Network) -> Result<Arc<Self>, Error> {
        // generate random salt for hashed password
        let mut salt: [u8; SALT_SIZE] = [0; SALT_SIZE];
        OsRng.fill_bytes(&mut salt);

        // generate hashed password which will be used as key to encrypt master_key
        debug!("hashing provided password");
        let hashed_password = hash_password(password, &salt)?;

        debug!("Creating storage for {}", name);
        let mut inner = Storage::new(name)?;

        // generate the Cipher
        let cipher = Cipher::new(&hashed_password, None)?;

        // save the salt used for password
        debug!("Save password salt in public storage");
        inner.set_password_salt(&salt)?;

        // generate the master key which is used for storage and then save it in encrypted form
        let mut master_key: [u8; 32] = [0; 32];
        OsRng.fill_bytes(&mut master_key);
        let encrypted_master_key = cipher.encrypt_value(&master_key)?;
        debug!("Save encrypted master key in public storage");
        inner.set_encrypted_master_key(&encrypted_master_key)?;
        
        // generate the storage salt and save it in encrypted form
        let mut storage_salt = [0; SALT_SIZE];
        OsRng.fill_bytes(&mut storage_salt);
        let encrypted_storage_salt = cipher.encrypt_value(&storage_salt)?;
        inner.set_encrypted_storage_salt(&encrypted_storage_salt)?;

        debug!("Creating encrypted storage");
        let mut storage = EncryptedStorage::new(inner, &master_key, storage_salt, network)?;

        // generate random keypair and save it to encrypted storage
        let keypair = if let Some(seed) = seed {
            debug!("Retrieving keypair from seed...");
            let words: Vec<String> = seed.split_whitespace().map(str::to_string).collect();
            let key = mnemonics::words_to_key(words)?;
            KeyPair::from_private_key(key)
        } else {
            debug!("Generating a new keypair...");
            KeyPair::new()
        };

        storage.set_keypair(&keypair)?;

        Ok(Self::new(storage, keypair, network))
    }

    pub fn open(name: String, password: String, network: Network) -> Result<Arc<Self>, Error> {
        debug!("Creating storage for {}", name);
        let storage = Storage::new(name)?;
        
        // get password salt for KDF
        debug!("Retrieving password salt from public storage");
        let salt = storage.get_password_salt()?;

        // retrieve encrypted master key from storage
        debug!("Retrieving encrypted master key from public storage");
        let encrypted_master_key = storage.get_encrypted_master_key()?;

        let hashed_password = hash_password(password, &salt)?;

        // decrypt the encrypted master key using the hashed password (used as key)
        let cipher = Cipher::new(&hashed_password, None)?;
        let master_key = cipher.decrypt_value(&encrypted_master_key).context("Invalid password provided for this wallet")?;

        // Retrieve the encrypted storage salt
        let encrypted_storage_salt = storage.get_encrypted_storage_salt()?;
        let storage_salt = cipher.decrypt_value(&encrypted_storage_salt).context("Invalid encrypted storage salt for this wallet")?;
        if storage_salt.len() != SALT_SIZE {
            error!("Invalid size received after decrypting storage salt: {} bytes", storage_salt.len());
            return Err(WalletError::InvalidSaltSize.into());
        }

        let mut salt: [u8; SALT_SIZE] = [0; SALT_SIZE];
        salt.copy_from_slice(&storage_salt);

        debug!("Creating encrypted storage");
        let storage = EncryptedStorage::new(storage, &master_key, salt, network)?;
        debug!("Retrieving keypair from encrypted storage");
        let keypair =  storage.get_keypair()?;

        Ok(Self::new(storage, keypair, network))
    }

    pub async fn set_password(&self, old_password: String, password: String) -> Result<(), Error> {
        let mut encrypted_storage = self.storage.write().await;
        let storage = encrypted_storage.get_mutable_public_storage();
        let (master_key, storage_salt) = {
            // retrieve old salt to build key from current password
            let salt = storage.get_password_salt()?;
            let hashed_password = hash_password(old_password, &salt)?;

            let encrypted_master_key = storage.get_encrypted_master_key()?;
            let encrypted_storage_salt = storage.get_encrypted_storage_salt()?;

            // decrypt the encrypted master key using the provided password
            let cipher = Cipher::new(&hashed_password, None)?;
            let master_key = cipher.decrypt_value(&encrypted_master_key).context("Invalid password provided")?;
            let storage_salt = cipher.decrypt_value(&encrypted_storage_salt)?;
            (master_key, storage_salt)
        };

        // generate a new salt for password
        let mut salt: [u8; SALT_SIZE] = [0; SALT_SIZE];
        OsRng.fill_bytes(&mut salt);

        // generate the password-based derivated key to encrypt the master key
        let hashed_password = hash_password(password, &salt)?;
        let cipher = Cipher::new(&hashed_password, None)?;

        // encrypt the master key using the new password
        let encrypted_key = cipher.encrypt_value(&master_key)?;

        // encrypt the salt with the new password
        let encrypted_storage_salt = cipher.encrypt_value(&storage_salt)?;

        // save on disk
        storage.set_password_salt(&salt)?;
        storage.set_encrypted_master_key(&encrypted_key)?;
        storage.set_encrypted_storage_salt(&encrypted_storage_salt)?;

        Ok(())
    }

    // create a transfer from the wallet to the given address to send the given amount of the given asset
    // and include extra data if present
    // TODO encrypt all the extra data for the receiver
    pub fn create_transfer(&self, storage: &EncryptedStorage, asset: Hash, key: PublicKey, extra_data: Option<DataType>, amount: u64) -> Result<Transfer, Error> {
        let balance = storage.get_balance_for(&asset).unwrap_or(0);
        // check if we have enough funds for this asset
        if amount > balance {
            return Err(WalletError::NotEnoughFunds(balance, amount, asset).into())
        }
        
        // include all extra data in the TX
        let extra_data = if let Some(data) = extra_data {
            let mut writer = Writer::new();
            data.write(&mut writer);

            // TODO encrypt all the extra data for the receiver
            // We can use XChaCha20 with 24 bytes 0 filled Nonce
            // this allow us to prevent saving nonce in it and save space
            // NOTE: We must be sure to have a different key each time

            if writer.total_write() > EXTRA_DATA_LIMIT_SIZE {
                return Err(WalletError::InvalidAddressParams.into())
            }
            Some(writer.bytes())
        } else {
            None
        };

        let transfer = Transfer {
            amount,
            asset: asset.clone(),
            to: key,
            extra_data
        };
        Ok(transfer)
    }

    // create the final transaction with calculated fees and signature
    // also check that we have enough funds for the transaction
    pub fn create_transaction(&self, storage: &EncryptedStorage, transaction_type: TransactionType) -> Result<Transaction, Error> {
        let nonce = storage.get_nonce().unwrap_or(0);
        let builder = TransactionBuilder::new(self.keypair.get_public_key().clone(), transaction_type, nonce, 1f64);
        let assets_spent: HashMap<&Hash, u64> = builder.total_spent();

        // check that we have enough balance for every assets spent
        for (asset, amount) in &assets_spent {
            let asset: &Hash = *asset;
            let balance = storage.get_balance_for(asset).unwrap_or(0);
            if balance < *amount {
                return Err(WalletError::NotEnoughFunds(balance, *amount, asset.clone()).into())
            }
        }

        // now we have to check that we have enough funds for spent + fees
        let total_native_spent = assets_spent.get(&XELIS_ASSET).unwrap_or(&0) +  builder.estimate_fees();
        let native_balance = storage.get_balance_for(&XELIS_ASSET).unwrap_or(0);
        if total_native_spent > native_balance {
            return Err(WalletError::NotEnoughFundsForFee(native_balance, total_native_spent).into())
        }

        Ok(builder.build(&self.keypair)?)
    }

    // submit a transaction to the network through the connection to daemon
    // returns error if the wallet is in offline mode
    pub async fn submit_transaction(&self, transaction: &Transaction) -> Result<(), WalletError> {
        let network_handler = self.network_handler.lock().await;
        if let Some(network_handler) = network_handler.as_ref() {
            network_handler.get_api().submit_transaction(transaction).await?;
            let mut storage = self.storage.write().await;
            storage.set_nonce(transaction.get_nonce() + 1)?;
            Ok(())
        } else {
            Err(WalletError::NotOnlineMode)
        }
    }

    // set wallet in online mode: start a communication task which will keep the wallet synced
    pub async fn set_online_mode(self: &Arc<Self>, daemon_address: &String) -> Result<(), Error> {
        if self.is_online().await {
            // user have to set in offline mode himself first
            return Err(WalletError::AlreadyOnlineMode.into())
        }

        // create the network handler
        let network_handler = NetworkHandler::new(Arc::clone(&self), daemon_address).await?;
        // start the task
        network_handler.start().await?;
        *self.network_handler.lock().await = Some(network_handler);

        Ok(())
    }

    // set wallet in offline mode: stop communication task if exists
    pub async fn set_offline_mode(&self) -> Result<(), WalletError> {
        let mut handler = self.network_handler.lock().await;
        if let Some(network_handler) = handler.take() {
            network_handler.stop().await;
        } else {
            return Err(WalletError::NotOnlineMode)
        }

        Ok(())
    }

    pub async fn rescan(&self, topoheight: u64) -> Result<(), WalletError> {
        if !self.is_online().await {
            // user have to set in offline mode himself first
            return Err(WalletError::AlreadyOnlineMode.into())
        }

        let handler = self.network_handler.lock().await;
        if let Some(network_handler) = handler.as_ref() {
            network_handler.stop().await;
            {
                let mut storage = self.get_storage().write().await;
                if topoheight >= storage.get_daemon_topoheight()? {
                    return Err(WalletError::RescanTopoheightTooHigh)
                }
                storage.set_daemon_topoheight(topoheight)?;
                storage.delete_top_block_hash()?;
                // balances will be re-fetched from daemon
                storage.delete_balances()?;
                if topoheight == 0 {
                    storage.delete_transactions()?;
                } else {
                    for tx in storage.get_transactions()? {
                        if tx.get_topoheight() > topoheight {
                            storage.delete_transaction(tx.get_hash())?;
                        }
                    }
                }
            }
            network_handler.start().await.context("Error while restarting network handler")?;
        } else {
            return Err(WalletError::NotOnlineMode)
        }

        Ok(())
    }

    pub async fn is_online(&self) -> bool {
        if let Some(network_handler) = self.network_handler.lock().await.as_ref() {
            network_handler.is_running().await
        } else {
            false
        }
    }

    // this function allow to user to get the network handler in case in want to stay in online mode
    // but want to pause / resume the syncing task through start/stop functions from it
    pub async fn get_network_handler(&self) -> &Mutex<Option<Arc<NetworkHandler>>> {
        &self.network_handler
    }

    pub fn get_address(&self) -> Address<'_> {
        self.keypair.get_public_key().to_address()
    }

    pub fn get_address_with(&self, data: DataType) -> Address<'_> {
        self.keypair.get_public_key().to_address_with(data)
    }

    pub fn get_seed(&self, language_index: usize) -> Result<String, Error> {
        let words = mnemonics::key_to_words(self.keypair.get_private_key(), language_index)?;
        Ok(words.join(" "))
    }

    pub fn get_storage(&self) -> &RwLock<EncryptedStorage> {
        &self.storage
    }

    pub fn get_network(&self) -> &Network {
        &self.network
    }
}