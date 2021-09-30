/// The interface for AccountsDb plugins. A plugin must implement
/// the AccountsDbPlugin trait to work with the runtime.
/// In addition, the dynamic library must export a "C" function _create_plugin which
/// creates the implementation of the plugin.
use {
    std::{any::Any, error, io},
    thiserror::Error,
};

impl Eq for ReplicaAccountInfo<'_> {}

#[derive(Clone, PartialEq, Debug)]
pub struct ReplicaAccountInfo<'a> {
    pub pubkey: &'a [u8],
    pub lamports: u64,
    pub owner: &'a [u8],
    pub executable: bool,
    pub rent_epoch: u64,
    pub data: &'a [u8],
}

pub enum ReplicaAccountInfoVersions<'a> {
    V0_0_1(&'a ReplicaAccountInfo<'a>),
}

#[derive(Error, Debug)]
pub enum AccountsDbPluginError {
    #[error("Error opening config file.")]
    ConfigFileOpenError(#[from] io::Error),

    #[error("Error reading config file.")]
    ConfigFileReadError { msg: String },

    #[error("Error updating account.")]
    AccountsUpdateError { msg: String },

    #[error("Error updating slot status.")]
    SlotStatusUpdateError { msg: String },

    #[error("Plugin-defined custom error.")]
    Custom(Box<dyn error::Error>),
}

#[derive(Debug, Clone)]
pub enum SlotStatus {
    Processed,
    Rooted,
    Confirmed,
}

impl SlotStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SlotStatus::Confirmed => "confirmed",
            SlotStatus::Processed => "processed",
            SlotStatus::Rooted => "rooted",
        }
    }
}

pub type Result<T> = std::result::Result<T, AccountsDbPluginError>;

pub trait AccountsDbPlugin: Any + Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// The callback called when a plugin is loaded by the system,
    /// used for doing whatever initialization is required by the plugin.
    /// The _config_file contains the name of the
    /// of the config file. The config must be in JSON format and
    /// include a field "libpath" indicating the full path
    /// name of the shared library implementing this interface.
    fn on_load(&mut self, _config_file: &str) -> Result<()> {
        Ok(())
    }

    /// The callback called right before a plugin is unloaded by the system
    /// Used for doing cleanup before unload.
    fn on_unload(&mut self) {}

    /// Called when an account is updated at a slot.
    fn update_account(&mut self, account: ReplicaAccountInfoVersions, slot: u64) -> Result<()>;

    /// Called when a slot status is updated
    fn update_slot_status(
        &mut self,
        slot: u64,
        parent: Option<u64>,
        status: SlotStatus,
    ) -> Result<()>;
}
