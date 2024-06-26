mod input;
mod session;

pub use self::input::{Input, InputContext};
use self::session::{Session, TEMP_SESSION_NAME};

use crate::client::{
    create_client_config, list_chat_models, list_client_types, ClientConfig, Model,
    OPENAI_COMPATIBLE_PLATFORMS,
};
use crate::function::{Function, ToolCallResult};
use crate::utils::{
    format_option_value, get_env_name, now, 
    set_text, 
};

use anyhow::{anyhow, bail, Context, Result};
use inquire::{Confirm, Select};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::{
    env,
    fs::{create_dir_all, read_dir, read_to_string, remove_file, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::exit,
    sync::Arc,
};
const CONFIG_FILE_NAME: &str = "config.yaml";
const MESSAGES_FILE_NAME: &str = "messages.md";
const SESSIONS_DIR_NAME: &str = "sessions";
const FUNCTIONS_DIR_NAME: &str = "functions";

const CLIENTS_FIELD: &str = "clients";

const SUMMARIZE_PROMPT: &str =
    "Summarize the discussion briefly in 200 words or less to use as a prompt for future context.";
const SUMMARY_PROMPT: &str = "This is a summary of the chat history as a recap: ";

const RAG_TEMPLATE: &str = r#"Answer the following question based only on the provided context:
<context>
__CONTEXT__
</context>

Question: __INPUT__
"#;

const LEFT_PROMPT: &str = "{color.green}{?session {session}{?role /}}{role}{?rag #{rag}}{color.cyan}{?session )}{!session >}{color.reset} ";
const RIGHT_PROMPT: &str = "{color.purple}{?session {?consume_tokens {consume_tokens}({consume_percent}%)}{!consume_tokens {consume_tokens}}}{color.reset}";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    #[serde(default)]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub save: bool,
    pub save_session: Option<bool>,
    pub function_calling: bool,
    pub clients: Vec<ClientConfig>,
    #[serde(skip)]
    pub session: Option<Session>,
    #[serde(skip)]
    pub model: Model,
    #[serde(skip)]
    pub function: Function,
    #[serde(skip)]
    pub last_message: Option<(Input, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_id: Default::default(),
            temperature: None,
            top_p: None,
            save: false,
            save_session: None,
            function_calling: false,
            clients: vec![],
            session: None,
            model: Default::default(),
            function: Default::default(),
            last_message: None,
        }
    }
}

pub type GlobalConfig = Arc<RwLock<Config>>;

impl Config {
    pub fn init() -> Result<Self> {
        let config_path = Self::config_file()?;

        let platform = env::var(get_env_name("platform")).ok();
        if !config_path.exists() {
            create_config_file(&config_path)?;
        }

        log::debug!("Loading config from {}", config_path.display());
        let mut config = if platform.is_some() {
            Self::load_config_env(&platform.unwrap())?
        } else {
            Self::load_config_file(&config_path)?
        };

        config.function = Function::init(&Self::functions_dir()?)?;

        config.setup_model()?;

        Ok(config)
    }

    pub fn config_dir() -> Result<PathBuf> {
        let env_name = get_env_name("config_dir");
        let path = if let Some(v) = env::var_os(env_name) {
            PathBuf::from(v)
        } else {
            let mut dir = dirs::config_dir().ok_or_else(|| anyhow!("Not found config dir"))?;
            dir.push(env!("CARGO_CRATE_NAME"));
            dir
        };
        Ok(path)
    }

    pub fn local_path(name: &str) -> Result<PathBuf> {
        let mut path = Self::config_dir()?;
        path.push(name);
        Ok(path)
    }

    pub fn save_message(
        &mut self,
        input: &mut Input,
        output: &str,
        tool_call_results: &[ToolCallResult],
    ) -> Result<()> {
        input.clear_patch_text();
        self.last_message = Some((input.clone(), output.to_string()));

        if let Some(session) = input.session_mut(&mut self.session) {
            session.add_message(input, output)?;
            return Ok(());
        }

        if !self.save {
            return Ok(());
        }
        let mut file = self.open_message_file()?;
        if output.is_empty() || !self.save {
            return Ok(());
        }
        let timestamp = now();
        let summary = input.summary();
        let input_markdown = input.render();
        let output = format!("# CHAT: {summary} [{timestamp}]\n{input_markdown}\n--------\n{output}\n--------\n\n",);
        file.write_all(output.as_bytes())
            .with_context(|| "Failed to save message")
    }

    pub fn config_file() -> Result<PathBuf> {
        match env::var(get_env_name("config_file")) {
            Ok(value) => Ok(PathBuf::from(value)),
            Err(_) => Self::local_path(CONFIG_FILE_NAME),
        }
    }

    pub fn messages_file() -> Result<PathBuf> {
        match env::var(get_env_name("messages_file")) {
            Ok(value) => Ok(PathBuf::from(value)),
            Err(_) => Self::local_path(MESSAGES_FILE_NAME),
        }
    }

    pub fn sessions_dir() -> Result<PathBuf> {
        match env::var(get_env_name("sessions_dir")) {
            Ok(value) => Ok(PathBuf::from(value)),
            Err(_) => Self::local_path(SESSIONS_DIR_NAME),
        }
    }

    pub fn functions_dir() -> Result<PathBuf> {
        match env::var(get_env_name("functions_dir")) {
            Ok(value) => Ok(PathBuf::from(value)),
            Err(_) => Self::local_path(FUNCTIONS_DIR_NAME),
        }
    }

    pub fn session_file(name: &str) -> Result<PathBuf> {
        let mut path = Self::sessions_dir()?;
        path.push(&format!("{name}.yaml"));
        Ok(path)
    }

    pub fn state(&self) -> StateFlags {
        let mut flags = StateFlags::empty();
        if let Some(session) = &self.session {
            if session.is_empty() {
                flags |= StateFlags::SESSION_EMPTY;
            } else {
                flags |= StateFlags::SESSION;
            }
        }
        flags
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        if let Some(session) = self.session.as_mut() {
            session.set_temperature(value);
        } else {
            self.temperature = value;
        }
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        if let Some(session) = self.session.as_mut() {
            session.set_top_p(value);
        } else {
            self.top_p = value;
        }
    }

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session(value);
        } else {
            self.save_session = value;
        }
    }

    pub fn set_model(&mut self, value: &str) -> Result<()> {
        let model = Model::find(&list_chat_models(self), value);
        match model {
            None => bail!("No model '{}'", value),
            Some(model) => {
                if let Some(session) = self.session.as_mut() {
                    session.set_model(&model);
                } 
                self.model = model;
                Ok(())
            }
        }
    }

    pub fn set_model_id(&mut self) {
        self.model_id = self.model.id()
    }

    pub fn restore_model(&mut self) -> Result<()> {
        let origin_model_id = self.model_id.clone();
        self.set_model(&origin_model_id)
    }

    pub fn system_info(&self) -> Result<String> {
        let display_path = |path: &Path| path.display().to_string();
        let (temperature, top_p) = if let Some(session) = &self.session {
            (session.temperature(), session.top_p())
        } else {
            (self.temperature, self.top_p)
        };
        let items = vec![
            ("model", self.model.id()),
            (
                "max_output_tokens",
                self.model
                    .max_tokens_param()
                    .map(|v| format!("{v} (current model)"))
                    .unwrap_or_else(|| "-".into()),
            ),
            ("temperature", format_option_value(&temperature)),
            ("top_p", format_option_value(&top_p)),
            ("function_calling", self.function_calling.to_string()),
            ("save", self.save.to_string()),
            ("save_session", format_option_value(&self.save_session)),
            ("config_file", display_path(&Self::config_file()?)),
            ("messages_file", display_path(&Self::messages_file()?)),
            ("sessions_dir", display_path(&Self::sessions_dir()?)),
            ("functions_dir", display_path(&Self::functions_dir()?)),
        ];
        let output = items
            .iter()
            .map(|(name, value)| format!("{name:<20}{value}"))
            .collect::<Vec<String>>()
            .join("\n");
        Ok(output)
    }

    pub fn info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            session.export()
        } else {
            self.system_info()
        }
    }

    pub fn last_reply(&self) -> &str {
        self.last_message
            .as_ref()
            .map(|(_, reply)| reply.as_str())
            .unwrap_or_default()
    }

    pub fn update(&mut self, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        let key = parts[0];
        let value = parts[1];
        match key {
            "max_output_tokens" => {
                let value = parse_value(value)?;
                self.model.set_max_tokens(value, true);
            }
            "temperature" => {
                let value = parse_value(value)?;
                self.set_temperature(value);
            }
            "top_p" => {
                let value = parse_value(value)?;
                self.set_top_p(value);
            }
            "function_calling" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.function_calling = value;
            }
            "save" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                self.save = value;
            }
            "save_session" => {
                let value = parse_value(value)?;
                self.set_save_session(value);
            }
            _ => bail!("Unknown key `{key}`"),
        }
        Ok(())
    }

    pub fn use_session(&mut self, session: Option<&str>) -> Result<()> {
        if self.session.is_some() {
            bail!(
                "Already in a session, please run '.exit session' first to exit the current session."
            );
        }
        match session {
            None => {
                let session_file = Self::session_file(TEMP_SESSION_NAME)?;
                if session_file.exists() {
                    remove_file(session_file).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_SESSION_NAME}' session")
                    })?;
                }
                let session = Session::new(self, TEMP_SESSION_NAME);
                self.session = Some(session);
            }
            Some(name) => {
                let session_path = Self::session_file(name)?;
                if !session_path.exists() {
                    self.session = Some(Session::new(self, name));
                } else {
                    let session = Session::load(name, &session_path)?;
                    let model_id = session.model_id().to_string();
                    self.session = Some(session);
                    self.set_model(&model_id)?;
                }
            }
        }
        if let Some(session) = self.session.as_mut() {
            if session.is_empty() {
                if let Some((input, output)) = &self.last_message {
                    let ans = Confirm::new(
                        "Start a session that incorporates the last question and answer?",
                    )
                    .with_default(false)
                    .prompt()?;
                    if ans {
                        session.add_message(input, output)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn exit_session(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            let sessions_dir = Self::sessions_dir()?;
            session.exit(&sessions_dir, false)?;
            self.last_message = None;
            self.restore_model()?;
        }
        Ok(())
    }

    pub fn save_session(&mut self, name: &str) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if !name.is_empty() {
                session.name = name.to_string();
            }
            let sessions_dir = Self::sessions_dir()?;
            session.save(&sessions_dir)?;
        }
        Ok(())
    }

    pub fn clear_session_messages(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.clear_messages();
        }
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<String> {
        let sessions_dir = match Self::sessions_dir() {
            Ok(dir) => dir,
            Err(_) => return vec![],
        };
        match read_dir(sessions_dir) {
            Ok(rd) => {
                let mut names = vec![];
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    if let Some(name) = name.to_string_lossy().strip_suffix(".yaml") {
                        names.push(name.to_string());
                    }
                }
                names.sort_unstable();
                names
            }
            Err(_) => vec![],
        }
    }


    pub fn is_compressing_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing)
            .unwrap_or_default()
    }

    pub fn end_compressing_session(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.compressing = false;
        }
    }

    fn generate_prompt_context(&self) -> HashMap<&str, String> {
        let mut output = HashMap::new();
        output.insert("model", self.model.id());
        output.insert("client_name", self.model.client_name().to_string());
        output.insert("model_name", self.model.name().to_string());
        output.insert(
            "max_input_tokens",
            self.model
                .max_input_tokens()
                .unwrap_or_default()
                .to_string(),
        );
        if let Some(temperature) = self.temperature {
            if temperature != 0.0 {
                output.insert("temperature", temperature.to_string());
            }
        }
        if let Some(top_p) = self.top_p {
            if top_p != 0.0 {
                output.insert("top_p", top_p.to_string());
            }
        }
        if self.save {
            output.insert("save", "true".to_string());
        }
        if let Some(session) = &self.session {
            output.insert("session", session.name().to_string());
            output.insert("dirty", session.dirty.to_string());
            let (tokens, percent) = session.tokens_and_percent();
            output.insert("consume_tokens", tokens.to_string());
            output.insert("consume_percent", percent.to_string());
            output.insert("user_messages_len", session.user_messages_len().to_string());
        }

        output
    }

    fn open_message_file(&self) -> Result<File> {
        let path = Self::messages_file()?;
        ensure_parent_exists(&path)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to create/append {}", path.display()))
    }

    fn load_config_file(config_path: &Path) -> Result<Self> {
        let content = read_to_string(config_path)
            .with_context(|| format!("Failed to load config at {}", config_path.display()))?;
        let config: Self = serde_yaml::from_str(&content).map_err(|err| {
            let err_msg = err.to_string();
            let err_msg = if err_msg.starts_with(&format!("{}: ", CLIENTS_FIELD)) {
                // location is incorrect, get rid of it
                err_msg
                    .split_once(" at line")
                    .map(|(v, _)| {
                        format!("{v} (Sorry for being unable to provide an exact location)")
                    })
                    .unwrap_or_else(|| "clients: invalid value".into())
            } else {
                err_msg
            };
            anyhow!("{err_msg}")
        })?;

        Ok(config)
    }

    fn load_config_env(platform: &str) -> Result<Self> {
        let model_id = match env::var(get_env_name("model_name")) {
            Ok(model_name) => format!("{platform}:{model_name}"),
            Err(_) => platform.to_string(),
        };
        let is_openai_compatible = OPENAI_COMPATIBLE_PLATFORMS
            .into_iter()
            .any(|(name, _)| platform == name);
        let client = if is_openai_compatible {
            json!({ "type": "openai-compatible", "name": platform })
        } else {
            json!({ "type": platform })
        };
        let config = json!({
            "model": model_id,
            "save": false,
            "clients": vec![client],
        });
        let config =
            serde_json::from_value(config).with_context(|| "Failed to load config from env")?;
        Ok(config)
    }

    fn setup_model(&mut self) -> Result<()> {
        let model_id = if self.model_id.is_empty() {
            let models = list_chat_models(self);
            if models.is_empty() {
                bail!("No available model");
            }

            models[0].id()
        } else {
            self.model_id.clone()
        };
        self.set_model(&model_id)?;
        self.model_id = model_id;
        Ok(())
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct StateFlags: u32 {
        const ROLE = 1 << 0;
        const SESSION_EMPTY = 1 << 1;
        const SESSION = 1 << 2;
        const RAG = 1 << 3;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssertState {
    True(StateFlags),
    False(StateFlags),
}

impl AssertState {
    pub fn any() -> Self {
        AssertState::False(StateFlags::empty())
    }
}

fn create_config_file(config_path: &Path) -> Result<()> {
    let ans = Confirm::new("No config file, create a new one?")
        .with_default(true)
        .prompt()?;
    if !ans {
        exit(0);
    }

    let client = Select::new("Platform:", list_client_types()).prompt()?;

    let mut config = serde_json::json!({});
    let (model, clients_config) = create_client_config(client)?;
    config["model"] = model.into();
    config[CLIENTS_FIELD] = clients_config;

    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;

    ensure_parent_exists(config_path)?;
    std::fs::write(config_path, config_data).with_context(|| "Failed to write to config file")?;
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(config_path, perms)?;
    }

    println!("✨ Saved config file to '{}'\n", config_path.display());

    Ok(())
}

pub(crate) fn ensure_parent_exists(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Failed to write to {}, No parent path", path.display()))?;
    if !parent.exists() {
        create_dir_all(parent).with_context(|| {
            format!(
                "Failed to write {}, Cannot create parent directory",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn set_bool(target: &mut bool, value: &str) {
    match value {
        "1" | "true" => *target = true,
        "0" | "false" => *target = false,
        _ => {}
    }
}

fn parse_value<T>(value: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
{
    let value = if value == "null" {
        None
    } else {
        let value = match value.parse() {
            Ok(value) => value,
            Err(_) => bail!("Invalid value '{}'", value),
        };
        Some(value)
    };
    Ok(value)
}

fn complete_bool(value: bool) -> Vec<String> {
    vec![(!value).to_string()]
}

fn complete_option_bool(value: Option<bool>) -> Vec<String> {
    match value {
        Some(true) => vec!["false".to_string(), "null".to_string()],
        Some(false) => vec!["true".to_string(), "null".to_string()],
        None => vec!["true".to_string(), "false".to_string()],
    }
}
