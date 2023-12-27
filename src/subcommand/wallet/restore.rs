use super::*;

#[derive(Debug, Parser, Clone)]
pub(crate) struct Restore {
  #[arg(help = "Restore wallet from <MNEMONIC>")]
  mnemonic: Mnemonic,
  #[arg(
    long,
    default_value = "",
    help = "Use <PASSPHRASE> when deriving wallet"
  )]
  pub(crate) passphrase: String,
}

impl Restore {
  pub(crate) fn run(self, options: Options) -> SubcommandResult {
    Wallet::initialize_wallet(&options, self.mnemonic.to_seed(self.passphrase))?;
    Ok(Box::new(Empty {}))
  }
}
