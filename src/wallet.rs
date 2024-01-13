use {
  super::*,
  bitcoin::secp256k1::{
    rand::{self, RngCore},
    All, Secp256k1,
  },
  bitcoin::{
    bip32::{ChildNumber, DerivationPath, ExtendedPrivKey, Fingerprint},
    Network,
  },
  bitcoincore_rpc::bitcoincore_rpc_json::{ImportDescriptors, Timestamp},
  fee_rate::FeeRate,
  http::StatusCode,
  miniscript::descriptor::{Descriptor, DescriptorSecretKey, DescriptorXKey, Wildcard},
  reqwest::{header, Url},
  transaction_builder::TransactionBuilder,
};

pub mod balance;
pub mod cardinals;
pub mod create;
pub mod etch;
pub mod inscribe;
pub mod inscriptions;
pub mod outputs;
pub mod receive;
pub mod restore;
pub mod sats;
pub mod send;
pub mod transaction_builder;
pub mod transactions;

#[derive(Debug, Parser)]
pub(crate) struct WalletCommand {
  #[arg(long, default_value = "ord", help = "Use wallet named <WALLET>.")]
  pub(crate) name: String,
  #[arg(long, alias = "nosync", help = "Do not update index.")]
  pub(crate) no_sync: bool,
  #[command(subcommand)]
  pub(crate) subcommand: Subcommand,
}

#[derive(Debug, Parser)]
pub(crate) enum Subcommand {
  #[command(about = "Get wallet balance")]
  Balance,
  #[command(about = "Create new wallet")]
  Create(create::Create),
  #[command(about = "Create rune")]
  Etch(etch::Etch),
  #[command(about = "Create inscription")]
  Inscribe(inscribe::Inscribe),
  #[command(about = "List wallet inscriptions")]
  Inscriptions,
  #[command(about = "Generate receive address")]
  Receive,
  #[command(about = "Restore wallet")]
  Restore(restore::Restore),
  #[command(about = "List wallet satoshis")]
  Sats(sats::Sats),
  #[command(about = "Send sat or inscription")]
  Send(send::Send),
  #[command(about = "See wallet transactions")]
  Transactions(transactions::Transactions),
  #[command(about = "List all unspent outputs in wallet")]
  Outputs,
  #[command(about = "List unspent cardinal outputs in wallet")]
  Cardinals,
}

impl WalletCommand {
  pub(crate) fn run(self, options: Options) -> SubcommandResult {
    let index = Arc::new(Index::open(&options)?);
    let handle = axum_server::Handle::new();
    LISTENERS.lock().unwrap().push(handle.clone());

    let ord_url: Url = {
      format!(
        "http://127.0.0.1:{}",
        TcpListener::bind("127.0.0.1:0")?.local_addr()?.port() // very hacky
      )
      .parse()
      .unwrap()
    };

    {
      let options = options.clone();
      let ord_url = ord_url.clone();
      std::thread::spawn(move || {
        crate::subcommand::server::Server {
          address: ord_url.host_str().map(|a| a.to_string()),
          acme_domain: vec![],
          csp_origin: None,
          http_port: ord_url.port(),
          https_port: None,
          acme_cache: None,
          acme_contact: vec![],
          http: true,
          https: false,
          redirect_http_to_https: false,
          enable_json_api: true,
          decompress: false,
          no_sync: self.no_sync,
        }
        .run(options, index, handle)
        .unwrap()
      });
    }

    let wallet = Wallet {
      no_sync: self.no_sync,
      options,
      ord_url,
      name: self.name.clone(),
    };

    let result = match self.subcommand {
      Subcommand::Balance => balance::run(wallet),
      Subcommand::Create(create) => create.run(wallet),
      Subcommand::Etch(etch) => etch.run(wallet),
      Subcommand::Inscribe(inscribe) => inscribe.run(wallet),
      Subcommand::Inscriptions => inscriptions::run(wallet),
      Subcommand::Receive => receive::run(wallet),
      Subcommand::Restore(restore) => restore.run(wallet),
      Subcommand::Sats(sats) => sats.run(wallet),
      Subcommand::Send(send) => send.run(wallet),
      Subcommand::Transactions(transactions) => transactions.run(wallet),
      Subcommand::Outputs => outputs::run(wallet),
      Subcommand::Cardinals => cardinals::run(wallet),
    };

    LISTENERS
      .lock()
      .unwrap()
      .iter()
      .for_each(|handle| handle.shutdown());

    result
  }
}

pub(crate) struct Wallet {
  pub(crate) name: String,
  pub(crate) no_sync: bool,
  pub(crate) options: Options, // Only need for bitcoin_rpc_client() and chain()
  pub(crate) ord_url: Url,
}

impl Wallet {
  pub(crate) fn bitcoin_client(&self) -> Result<Client> {
    let client = check_version(self.options.bitcoin_rpc_client(Some(self.name.clone()))?)?;

    if !client.list_wallets()?.contains(&self.name) {
      client.load_wallet(&self.name)?;
    }

    let descriptors = client.list_descriptors(None)?.descriptors;

    let tr = descriptors
      .iter()
      .filter(|descriptor| descriptor.desc.starts_with("tr("))
      .count();

    let rawtr = descriptors
      .iter()
      .filter(|descriptor| descriptor.desc.starts_with("rawtr("))
      .count();

    if tr != 2 || descriptors.len() != 2 + rawtr {
      bail!("wallet \"{}\" contains unexpected output descriptors, and does not appear to be an `ord` wallet, create a new wallet with `ord wallet create`", self.name);
    }

    Ok(client)
  }

  pub(crate) fn ord_client(&self) -> Result<reqwest::blocking::Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
      header::ACCEPT,
      header::HeaderValue::from_static("application/json"),
    );

    let client = reqwest::blocking::ClientBuilder::new()
      .default_headers(headers)
      .build()
      .map_err(|err| anyhow!(err))?;

    let chain_block_count = self.bitcoin_client()?.get_block_count().unwrap() + 1;

    if !self.no_sync {
      for i in 0.. {
        let response = client
          .get(self.ord_url.join("/blockcount").unwrap())
          .send()?;

        assert_eq!(response.status(), StatusCode::OK);

        if response.text()?.parse::<u64>().unwrap() >= chain_block_count {
          break;
        } else if i == 20 {
          panic!("wallet failed to synchronize to index");
        }

        thread::sleep(Duration::from_millis(25));
      }
    }

    Ok(client)
  }

  fn get_output(&self, output: &OutPoint) -> Result<OutputJson> {
    let response = self
      .ord_client()?
      .get(self.ord_url.join(&format!("/output/{output}")).unwrap())
      .send()?;

    let output_json: OutputJson = serde_json::from_str(&response.text()?)?;

    if !output_json.indexed {
      bail!("output in Bitcoin Core wallet but not in ord index: {output}");
    }

    Ok(output_json)
  }

  pub(crate) fn get_unspent_outputs(&self) -> Result<BTreeMap<OutPoint, Amount>> {
    let mut utxos = BTreeMap::new();
    utxos.extend(
      self
        .bitcoin_client()?
        .list_unspent(None, None, None, None, None)?
        .into_iter()
        .map(|utxo| {
          let outpoint = OutPoint::new(utxo.txid, utxo.vout);
          let amount = utxo.amount;

          (outpoint, amount)
        }),
    );

    let locked_utxos: BTreeSet<OutPoint> = self.get_locked_outputs()?;

    for outpoint in locked_utxos {
      utxos.insert(
        outpoint,
        Amount::from_sat(
          self
            .bitcoin_client()?
            .get_raw_transaction(&outpoint.txid, None)?
            .output[TryInto::<usize>::try_into(outpoint.vout).unwrap()]
          .value,
        ),
      );
    }

    for output in utxos.keys() {
      self.get_output(output)?; //check that wallet outputs in ord index
    }

    Ok(utxos)
  }

  pub(crate) fn get_output_sat_ranges(&self) -> Result<Vec<(OutPoint, Vec<(u64, u64)>)>> {
    ensure!(
      self.check_sat_index()?,
      "index must be built with `--index-sats` to use `--sat`"
    );

    let mut output_sat_ranges = Vec::new();
    for output in self.get_unspent_outputs()?.keys() {
      if let Some(sat_ranges) = self.get_output(output)?.sat_ranges {
        output_sat_ranges.push((*output, sat_ranges));
      } else {
        bail!("output {output} in wallet but is spent according to index");
      }
    }

    Ok(output_sat_ranges)
  }

  pub(crate) fn find_sat_in_outputs(
    &self,
    sat: Sat,
    utxos: &BTreeMap<OutPoint, Amount>,
  ) -> Result<SatPoint> {
    ensure!(
      self.check_sat_index()?,
      "index must be built with `--index-sats` to use `--sat`"
    );

    for output in utxos.keys() {
      if let Some(sat_ranges) = self.get_output(output)?.sat_ranges {
        let mut offset = 0;
        for (start, end) in sat_ranges {
          if start <= sat.n() && sat.n() < end {
            return Ok(SatPoint {
              outpoint: *output,
              offset: offset + sat.n() - start,
            });
          }
          offset += end - start;
        }
      } else {
        continue;
      }
    }

    Err(anyhow!(format!(
      "could not find sat `{sat}` in wallet outputs"
    )))
  }

  fn get_inscription(&self, inscription_id: InscriptionId) -> Result<InscriptionJson> {
    let response = self
      .ord_client()?
      .get(
        self
          .ord_url
          .join(&format!("/inscription/{inscription_id}"))
          .unwrap(),
      )
      .send()?;

    if response.status().is_client_error() {
      bail!("inscription {inscription_id} not found");
    }

    Ok(serde_json::from_str(&response.text()?)?)
  }

  pub(crate) fn get_inscriptions(&self) -> Result<BTreeMap<SatPoint, InscriptionId>> {
    let mut inscriptions = BTreeMap::new();
    for output in self.get_unspent_outputs()?.keys() {
      for inscription in self.get_output(output)?.inscriptions {
        inscriptions.insert(self.get_inscription_satpoint(inscription)?, inscription);
      }
    }

    Ok(inscriptions)
  }

  pub(crate) fn get_inscription_satpoint(&self, inscription_id: InscriptionId) -> Result<SatPoint> {
    Ok(self.get_inscription(inscription_id)?.satpoint)
  }

  pub(crate) fn get_rune(
    &self,
    rune: Rune,
  ) -> Result<Option<(RuneId, RuneEntry, Option<InscriptionId>)>> {
    let response = self
      .ord_client()?
      .get(
        self
          .ord_url
          .join(&format!("/rune/{}", SpacedRune { rune, spacers: 0 }))
          .unwrap(),
      )
      .send()?;

    if response.status().is_client_error() {
      return Ok(None);
    }

    let rune_json: RuneJson = serde_json::from_str(&response.text()?)?;

    Ok(Some((rune_json.id, rune_json.entry, rune_json.parent)))
  }

  pub(crate) fn get_runic_outputs(&self) -> Result<BTreeSet<OutPoint>> {
    let mut runic_outputs = BTreeSet::new();
    for output in self.get_unspent_outputs()?.keys() {
      if !self.get_output(output)?.runes.is_empty() {
        runic_outputs.insert(*output);
      }
    }

    Ok(runic_outputs)
  }

  pub(crate) fn get_runes_balances_for_output(
    &self,
    output: &OutPoint,
  ) -> Result<Vec<(SpacedRune, Pile)>> {
    Ok(self.get_output(output)?.runes)
  }

  pub(crate) fn get_rune_balance_in_output(&self, output: &OutPoint, rune: Rune) -> Result<u128> {
    Ok(
      self
        .get_runes_balances_for_output(output)?
        .iter()
        .map(|(spaced_rune, pile)| {
          if spaced_rune.rune == rune {
            pile.amount
          } else {
            0
          }
        })
        .sum(),
    )
  }

  pub(crate) fn get_locked_outputs(&self) -> Result<BTreeSet<OutPoint>> {
    #[derive(Deserialize)]
    pub(crate) struct JsonOutPoint {
      txid: bitcoin::Txid,
      vout: u32,
    }

    Ok(
      self
        .bitcoin_client()?
        .call::<Vec<JsonOutPoint>>("listlockunspent", &[])?
        .into_iter()
        .map(|outpoint| OutPoint::new(outpoint.txid, outpoint.vout))
        .collect(),
    )
  }

  pub(crate) fn get_change_address(&self) -> Result<Address> {
    Ok(
      self
        .bitcoin_client()?
        .call::<Address<NetworkUnchecked>>("getrawchangeaddress", &["bech32m".into()])
        .context("could not get change addresses from wallet")?
        .require_network(self.chain().network())?,
    )
  }

  pub(crate) fn get_server_status(&self) -> Result<StatusJson> {
    let status: StatusJson = serde_json::from_str(
      &self
        .ord_client()?
        .get(self.ord_url.join("/status").unwrap())
        .send()?
        .text()?,
    )?;

    Ok(status)
  }

  pub(crate) fn check_rune_index(&self) -> Result<bool> {
    Ok(self.get_server_status()?.rune_index)
  }

  pub(crate) fn check_sat_index(&self) -> Result<bool> {
    Ok(self.get_server_status()?.sat_index)
  }

  pub(crate) fn chain(&self) -> Chain {
    self.options.chain()
  }

  pub(crate) fn initialize(&self, seed: [u8; 64]) -> Result {
    check_version(self.options.bitcoin_rpc_client(None)?)?.create_wallet(
      &self.name,
      None,
      Some(true),
      None,
      None,
    )?;

    let network = self.chain().network();

    let secp = Secp256k1::new();

    let master_private_key = ExtendedPrivKey::new_master(network, &seed)?;

    let fingerprint = master_private_key.fingerprint(&secp);

    let derivation_path = DerivationPath::master()
      .child(ChildNumber::Hardened { index: 86 })
      .child(ChildNumber::Hardened {
        index: u32::from(network != Network::Bitcoin),
      })
      .child(ChildNumber::Hardened { index: 0 });

    let derived_private_key = master_private_key.derive_priv(&secp, &derivation_path)?;

    for change in [false, true] {
      self.derive_and_import_descriptor(
        &secp,
        (fingerprint, derivation_path.clone()),
        derived_private_key,
        change,
      )?;
    }

    Ok(())
  }

  fn derive_and_import_descriptor(
    &self,
    secp: &Secp256k1<All>,
    origin: (Fingerprint, DerivationPath),
    derived_private_key: ExtendedPrivKey,
    change: bool,
  ) -> Result {
    let secret_key = DescriptorSecretKey::XPrv(DescriptorXKey {
      origin: Some(origin),
      xkey: derived_private_key,
      derivation_path: DerivationPath::master().child(ChildNumber::Normal {
        index: change.into(),
      }),
      wildcard: Wildcard::Unhardened,
    });

    let public_key = secret_key.to_public(secp)?;

    let mut key_map = std::collections::HashMap::new();
    key_map.insert(public_key.clone(), secret_key);

    let desc = Descriptor::new_tr(public_key, None)?;

    self
      .options
      .bitcoin_rpc_client(Some(self.name.clone()))?
      .import_descriptors(ImportDescriptors {
        descriptor: desc.to_string_with_secret(&key_map),
        timestamp: Timestamp::Now,
        active: Some(true),
        range: None,
        next_index: None,
        internal: Some(change),
        label: None,
      })?;

    Ok(())
  }
}

pub(crate) fn check_version(client: Client) -> Result<Client> {
  const MIN_VERSION: usize = 240000;

  let bitcoin_version = client.version()?;
  if bitcoin_version < MIN_VERSION {
    bail!(
      "Bitcoin Core {} or newer required, current version is {}",
      format_bitcoin_core_version(MIN_VERSION),
      format_bitcoin_core_version(bitcoin_version),
    );
  } else {
    Ok(client)
  }
}

fn format_bitcoin_core_version(version: usize) -> String {
  format!(
    "{}.{}.{}",
    version / 10000,
    version % 10000 / 100,
    version % 100
  )
}