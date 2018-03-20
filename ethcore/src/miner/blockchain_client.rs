// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::fmt;
use std::collections::HashMap;

use ethereum_types::{H256, U256, Address};
use ethcore_miner::pool;
use ethcore_miner::pool::client::NonceClient;
use transaction::{
	self,
	UnverifiedTransaction,
	SignedTransaction,
};
use parking_lot::RwLock;

use account_provider::AccountProvider;
use client::{TransactionId, BlockInfo, CallContract, Nonce};
use engines::EthEngine;
use header::Header;
use miner;
use miner::service_transaction_checker::ServiceTransactionChecker;

type NoncesCache = RwLock<HashMap<Address, U256>>;

const MAX_NONCE_CACHE_SIZE: usize = 4096;
const EXPECTED_NONCE_CACHE_SIZE: usize = 2048;

pub struct BlockChainClient<'a, C: 'a> {
	chain: &'a C,
	cached_nonces: CachedNonceClient<'a, C>,
	engine: &'a EthEngine,
	accounts: Option<&'a AccountProvider>,
	service_transaction_checker: Option<ServiceTransactionChecker>,
}

impl<'a, C: 'a> Clone for BlockChainClient<'a, C> {
	fn clone(&self) -> Self {
		BlockChainClient {
			chain: self.chain,
			cached_nonces: self.cached_nonces.clone(),
			engine: self.engine,
			accounts: self.accounts.clone(),
			service_transaction_checker: self.service_transaction_checker.clone(),
		}
	}
}

impl<'a, C: 'a> BlockChainClient<'a, C> where
C: BlockInfo + CallContract,
{
	pub fn new(
		chain: &'a C,
		cache: &'a NoncesCache,
		engine: &'a EthEngine,
		accounts: Option<&'a AccountProvider>,
		refuse_service_transactions: bool,
	) -> Self {
		BlockChainClient {
			chain,
			cached_nonces: CachedNonceClient::new(chain, cache),
			engine,
			accounts,
			service_transaction_checker: if refuse_service_transactions {
				None
			} else {
				Some(Default::default())
			},
		}
	}

	pub fn verify_signed(&self, tx: &SignedTransaction) -> Result<(), transaction::Error> {
		let best_block_header = self.chain.best_block_header().decode();
		self.verify_signed_transaction(tx, &best_block_header)
	}

	pub fn verify_signed_transaction(&self, tx: &SignedTransaction, header: &Header) -> Result<(), transaction::Error> {
		self.engine.machine().verify_transaction(&tx, header, self.chain)
	}
}

impl<'a, C: 'a> fmt::Debug for BlockChainClient<'a, C> {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		write!(fmt, "BlockChainClient")
	}
}

impl<'a, C: 'a> pool::client::Client for BlockChainClient<'a, C> where
	C: miner::TransactionVerifierClient + Sync,
{
	fn transaction_already_included(&self, hash: &H256) -> bool {
		self.chain.transaction_block(TransactionId::Hash(*hash)).is_some()
	}

	fn verify_transaction(&self, tx: UnverifiedTransaction)-> Result<SignedTransaction, transaction::Error> {
		let best_block_header = self.chain.best_block_header().decode();
		self.engine.verify_transaction_basic(&tx, &best_block_header)?;
		let tx = self.engine.verify_transaction_unordered(tx, &best_block_header)?;

		self.verify_signed_transaction(&tx, &best_block_header)?;

		Ok(tx)
	}

	fn account_details(&self, address: &Address) -> pool::client::AccountDetails {
		pool::client::AccountDetails {
			nonce: self.cached_nonces.account_nonce(address),
			balance: self.chain.latest_balance(address),
			is_local: self.accounts.map_or(false, |accounts| accounts.has_account(*address).unwrap_or(false)),
		}
	}

	fn required_gas(&self, tx: &transaction::Transaction) -> U256 {
		tx.gas_required(&self.chain.latest_schedule()).into()
	}

	fn transaction_type(&self, tx: &SignedTransaction) -> pool::client::TransactionType {
		match self.service_transaction_checker {
			None => pool::client::TransactionType::Regular,
			Some(ref checker) => match checker.check(self.chain, &tx) {
				Ok(true) => pool::client::TransactionType::Service,
				Ok(false) => pool::client::TransactionType::Regular,
				Err(e) => {
					debug!(target: "txqueue", "Unable to verify service transaction: {:?}", e);
					pool::client::TransactionType::Regular
				},
			}
		}
	}
}

impl<'a, C: 'a> NonceClient for BlockChainClient<'a, C> where
	C: Nonce + Sync,
{
	fn account_nonce(&self, address: &Address) -> U256 {
		self.cached_nonces.account_nonce(address)
	}
}

pub struct CachedNonceClient<'a, C: 'a> {
	client: &'a C,
	cache: &'a NoncesCache,
}

impl<'a, C: 'a> Clone for CachedNonceClient<'a, C> {
	fn clone(&self) -> Self {
		CachedNonceClient {
			client: self.client,
			cache: self.cache,
		}
	}
}

impl<'a, C: 'a> fmt::Debug for CachedNonceClient<'a, C> {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		fmt.debug_struct("CachedNonceClient")
			.field("cache", &self.cache.read().len())
			.finish()
	}
}

impl<'a, C: 'a> CachedNonceClient<'a, C> {
	pub fn new(client: &'a C, cache: &'a NoncesCache) -> Self {
		CachedNonceClient {
			client,
			cache,
		}
	}
}

impl<'a, C: 'a> NonceClient for CachedNonceClient<'a, C> where
	C: Nonce + Sync,
{
  fn account_nonce(&self, address: &Address) -> U256 {
	  if let Some(nonce) = self.cache.read().get(address) {
		  return *nonce;
	  }

	  // We don't check again if cache has been populated.
	  // It's not THAT expensive to fetch the nonce from state.
	  let mut cache = self.cache.write();
	  let nonce = self.client.latest_nonce(address);
	  cache.insert(*address, nonce);

	  if cache.len() < MAX_NONCE_CACHE_SIZE {
		  return nonce
	  }

	  // Remove excessive amount of entries from the cache
	  while cache.len() > EXPECTED_NONCE_CACHE_SIZE {
		  // Just remove random entry
		  if let Some(key) = cache.keys().next().cloned() {
			  cache.remove(&key);
		  }
	  }
	  nonce
  }
}
