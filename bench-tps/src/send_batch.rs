use {
    crate::bench_tps_client::*,
    log::*,
    rayon::prelude::*,
    solana_core::gen_keys::GenKeys,
    solana_measure::measure::Measure,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        hash::Hash,
        message::Message,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        signer::signers::Signers,
        system_instruction,
        transaction::Transaction,
    },
    std::{
        collections::HashSet,
        marker::Send,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        thread::sleep,
        time::{Duration, Instant},
    },
};

pub fn get_latest_blockhash<T: BenchTpsClient>(client: &T) -> Hash {
    loop {
        match client.get_latest_blockhash_with_commitment(CommitmentConfig::processed()) {
            Ok((blockhash, _)) => return blockhash,
            Err(err) => {
                info!("Couldn't get last blockhash: {:?}", err);
                sleep(Duration::from_secs(1));
            }
        };
    }
}

pub fn generate_keypairs(seed_keypair: &Keypair, count: u64) -> (Vec<Keypair>, u64) {
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_keypair.to_bytes()[..32]);
    let mut rnd = GenKeys::new(seed);

    let mut total_keys = 0;
    let mut extra = 0; // This variable tracks the number of keypairs needing extra transaction fees funded
    let mut delta = 1;
    while total_keys < count {
        extra += delta;
        delta *= MAX_SPENDS_PER_TX;
        total_keys += delta;
    }
    (rnd.gen_n_keypairs(total_keys), extra)
}

/// fund the dests keys by spending all of the source keys into MAX_SPENDS_PER_TX
/// on every iteration.  This allows us to replay the transfers because the source is either empty,
/// or full
pub fn fund_keys<T: 'static + BenchTpsClient + Send + Sync>(
    client: Arc<T>,
    source: &Keypair,
    dests: &[Keypair],
    total: u64,
    max_fee: u64,
    lamports_per_account: u64,
) {
    let mut funded: Vec<&Keypair> = vec![source];
    let mut funded_funds = total;
    let mut not_funded: Vec<&Keypair> = dests.iter().collect();
    while !not_funded.is_empty() {
        // Build to fund list and prepare funding sources for next iteration
        let mut new_funded: Vec<&Keypair> = vec![];
        let mut to_fund: Vec<(&Keypair, Vec<(Pubkey, u64)>)> = vec![];
        let to_lamports = (funded_funds - lamports_per_account - max_fee) / MAX_SPENDS_PER_TX;
        for f in funded {
            let start = not_funded.len() - MAX_SPENDS_PER_TX as usize;
            let dests: Vec<_> = not_funded.drain(start..).collect();
            let spends: Vec<_> = dests.iter().map(|k| (k.pubkey(), to_lamports)).collect();
            to_fund.push((f, spends));
            new_funded.extend(dests.into_iter());
        }

        to_fund.chunks(FUND_CHUNK_LEN).for_each(|chunk| {
            Vec::<(&Keypair, Transaction)>::with_capacity(chunk.len()).fund(
                &client,
                chunk,
                to_lamports,
            );
        });

        info!("funded: {} left: {}", new_funded.len(), not_funded.len());
        funded = new_funded;
        funded_funds = to_lamports;
    }
}

const MAX_SPENDS_PER_TX: u64 = 4;

// Size of the chunk of transactions
// try to transfer a "few" at a time with recent blockhash
// assume 4MB network buffers, and 512 byte packets
const FUND_CHUNK_LEN: usize = 4 * 1024 * 1024 / 512;

fn verify_funding_transfer<T: BenchTpsClient>(
    client: &Arc<T>,
    tx: &Transaction,
    amount: u64,
) -> bool {
    for a in &tx.message().account_keys[1..] {
        match client.get_balance_with_commitment(a, CommitmentConfig::processed()) {
            Ok(balance) => return balance >= amount,
            Err(err) => error!("failed to get balance {:?}", err),
        }
    }
    false
}

trait SendBatchTransactions<'a, T: Sliceable + Send + Sync> {
    fn sign(&mut self, blockhash: Hash);
    fn send<C: BenchTpsClient>(&self, client: &Arc<C>);
    fn verify<C: 'static + BenchTpsClient + Send + Sync>(
        &mut self,
        client: &Arc<C>,
        to_lamports: u64,
    );
}

/// This trait allows reuse SendBatchTransactions to send
/// transactions which require more than one signature
trait Sliceable {
    type Slice;
    fn as_slice(&self) -> Self::Slice;
    // Pubkey used as unique id to identify verified transactions
    fn get_pubkey(&self) -> Pubkey;
}

impl<'a, T: Sliceable + Send + Sync> SendBatchTransactions<'a, T> for Vec<(T, Transaction)>
where
    <T as Sliceable>::Slice: Signers,
{
    fn sign(&mut self, blockhash: Hash) {
        let mut sign_txs = Measure::start("sign_txs");
        self.par_iter_mut().for_each(|(k, tx)| {
            tx.sign(&k.as_slice(), blockhash);
        });
        sign_txs.stop();
        debug!("sign {} txs: {}us", self.len(), sign_txs.as_us());
    }

    fn send<C: BenchTpsClient>(&self, client: &Arc<C>) {
        let mut send_txs = Measure::start("send_and_clone_txs");
        let batch: Vec<_> = self.iter().map(|(_keypair, tx)| tx.clone()).collect();
        client.send_batch(batch).expect("transfer");
        send_txs.stop();
        debug!("send {} {}", self.len(), send_txs);
    }

    fn verify<C: 'static + BenchTpsClient + Send + Sync>(
        &mut self,
        client: &Arc<C>,
        to_lamports: u64,
    ) {
        let starting_txs = self.len();
        let verified_txs = Arc::new(AtomicUsize::new(0));
        let too_many_failures = Arc::new(AtomicBool::new(false));
        let loops = if starting_txs < 1000 { 3 } else { 1 };
        // Only loop multiple times for small (quick) transaction batches
        let time = Arc::new(Mutex::new(Instant::now()));
        for _ in 0..loops {
            let time = time.clone();
            let failed_verify = Arc::new(AtomicUsize::new(0));
            let client = client.clone();
            let verified_txs = &verified_txs;
            let failed_verify = &failed_verify;
            let too_many_failures = &too_many_failures;
            let verified_set: HashSet<Pubkey> = self
                .par_iter()
                .filter_map(move |(k, tx)| {
                    let pubkey = k.get_pubkey();
                    if too_many_failures.load(Ordering::Relaxed) {
                        return None;
                    }

                    let verified = if verify_funding_transfer(&client, tx, to_lamports) {
                        verified_txs.fetch_add(1, Ordering::Relaxed);
                        Some(pubkey)
                    } else {
                        failed_verify.fetch_add(1, Ordering::Relaxed);
                        None
                    };

                    let verified_txs = verified_txs.load(Ordering::Relaxed);
                    let failed_verify = failed_verify.load(Ordering::Relaxed);
                    let remaining_count = starting_txs.saturating_sub(verified_txs + failed_verify);
                    if failed_verify > 100 && failed_verify > verified_txs {
                        too_many_failures.store(true, Ordering::Relaxed);
                        warn!(
                            "Too many failed transfers... {} remaining, {} verified, {} failures",
                            remaining_count, verified_txs, failed_verify
                        );
                    }
                    if remaining_count > 0 {
                        let mut time_l = time.lock().unwrap();
                        if time_l.elapsed().as_secs() > 2 {
                            info!(
                                "Verifying transfers... {} remaining, {} verified, {} failures",
                                remaining_count, verified_txs, failed_verify
                            );
                            *time_l = Instant::now();
                        }
                    }

                    verified
                })
                .collect();

            self.retain(|(k, _)| !verified_set.contains(&k.get_pubkey()));
            if self.is_empty() {
                break;
            }
            info!("Looping verifications");

            let verified_txs = verified_txs.load(Ordering::Relaxed);
            let failed_verify = failed_verify.load(Ordering::Relaxed);
            let remaining_count = starting_txs.saturating_sub(verified_txs + failed_verify);
            info!(
                "Verifying transfers... {} remaining, {} verified, {} failures",
                remaining_count, verified_txs, failed_verify
            );
            sleep(Duration::from_millis(100));
        }
    }
}

type FundingSigners<'a> = &'a Keypair;
type FundingChunk<'a> = [(FundingSigners<'a>, Vec<(Pubkey, u64)>)];
type FundingContainer<'a> = Vec<(FundingSigners<'a>, Transaction)>;

impl<'a> Sliceable for FundingSigners<'a> {
    type Slice = [FundingSigners<'a>; 1];
    fn as_slice(&self) -> Self::Slice {
        [self]
    }
    fn get_pubkey(&self) -> Pubkey {
        self.pubkey()
    }
}

trait FundingTransactions<'a>: SendBatchTransactions<'a, FundingSigners<'a>> {
    fn fund<T: 'static + BenchTpsClient + Send + Sync>(
        &mut self,
        client: &Arc<T>,
        to_fund: &FundingChunk<'a>,
        to_lamports: u64,
    );
    fn make(&mut self, to_fund: &FundingChunk<'a>);
}

impl<'a> FundingTransactions<'a> for FundingContainer<'a> {
    fn fund<T: 'static + BenchTpsClient + Send + Sync>(
        &mut self,
        client: &Arc<T>,
        to_fund: &FundingChunk<'a>,
        to_lamports: u64,
    ) {
        self.make(to_fund);

        let mut tries = 0;
        while !self.is_empty() {
            info!(
                "{} {} each to {} accounts in {} txs",
                if tries == 0 {
                    "transferring"
                } else {
                    " retrying"
                },
                to_lamports,
                self.len() * MAX_SPENDS_PER_TX as usize,
                self.len(),
            );

            let blockhash = get_latest_blockhash(client.as_ref());

            // re-sign retained to_fund_txes with updated blockhash
            self.sign(blockhash);
            self.send(client);

            // Sleep a few slots to allow transactions to process
            sleep(Duration::from_secs(1));

            self.verify(client, to_lamports);

            // retry anything that seems to have dropped through cracks
            //  again since these txs are all or nothing, they're fine to
            //  retry
            tries += 1;
        }
        info!("transferred");
    }

    fn make(&mut self, to_fund: &FundingChunk<'a>) {
        let mut make_txs = Measure::start("make_txs");
        let to_fund_txs: FundingContainer<'a> = to_fund
            .par_iter()
            .map(|(k, t)| {
                let instructions = system_instruction::transfer_many(&k.pubkey(), t);
                let message = Message::new(&instructions, Some(&k.pubkey()));
                (*k, Transaction::new_unsigned(message))
            })
            .collect();
        make_txs.stop();
        debug!(
            "make {} unsigned txs: {}us",
            to_fund_txs.len(),
            make_txs.as_us()
        );
        self.extend(to_fund_txs);
    }
}
