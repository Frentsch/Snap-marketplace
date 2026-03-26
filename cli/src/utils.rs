use sui_config::{SUI_CLIENT_CONFIG, SUI_KEYSTORE_FILENAME, sui_config_dir};
use sui_sdk::wallet_context::WalletContext;
use sui_keys::keystore::{FileBasedKeystore, Keystore};
use sui_types::base_types::SuiAddress;
use anyhow::Result;

pub async fn get_wallet() -> Result<WalletContext> {
    let conf_path = sui_config_dir()?.join(SUI_CLIENT_CONFIG);
    let keystore_path = sui_config_dir()?.join(SUI_KEYSTORE_FILENAME);

    // WalletContext::new reads the keystore path from inside client.yaml,
    // which is a Linux path and fails to resolve on Windows. Load the keystore
    // explicitly using the env-variable-resolved path, then inject it.
    let keystore = FileBasedKeystore::load_or_create(&keystore_path)?;

    let mut wallet = WalletContext::new(&conf_path)?
        .with_request_timeout(std::time::Duration::from_secs(60));

    wallet.config.keystore = Keystore::File(keystore);

    Ok(wallet)
}

pub async fn get_second_address() -> Result<SuiAddress> {
    let mut wallet = get_wallet().await?;
    let active_address = wallet.active_address()?;
    let addresses = wallet.get_addresses();
    let addresses = addresses.into_iter().filter(|address| address != &active_address).collect::<Vec<_>>();
    let recipient = addresses.first().expect("must have second address");
    Ok(*recipient)
}
