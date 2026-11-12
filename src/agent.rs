use std::path::Path;

use crate::Result;
use crate::config::Config;
use crate::provider::{Event, Provider};
use crate::session::{Session, Usage};

pub struct Agent {
    provider: Provider,
    session: Session,
    system: String,
    effort: Option<String>,
}

impl Agent {
    pub fn new(cfg: &Config, cwd: &str, session_path: &Path) -> Result<Agent> {
        let session = Session::open(
            session_path,
            cwd,
            &cfg.model,
            cfg.effort.as_deref().unwrap_or("auto"),
        )?;
        Ok(Agent {
            provider: Provider::new(cfg),
            session,
            system: cfg.system_prompt(cwd),
            effort: cfg.effort.clone(),
        })
    }

    /// 一轮对话：追加用户消息 → 请求模型 → 追加回答。
    pub fn submit(&mut self, prompt: &str, on: &mut dyn FnMut(Event)) -> Result<Usage> {
        self.session.append_user(prompt)?;
        let messages = self.session.messages();
        let reply = self
            .provider
            .stream(&self.system, &messages, self.effort.as_deref(), on)?;
        self.session
            .append_assistant(reply.content, reply.reasoning, reply.usage.clone())?;
        Ok(reply.usage)
    }

    pub fn set_effort(&mut self, effort: Option<String>) -> Result<()> {
        self.session
            .append_effort(effort.as_deref().unwrap_or("auto"))?;
        self.effort = effort;
        Ok(())
    }

    pub fn new_session(&mut self) -> Result<std::path::PathBuf> {
        self.session.archive()
    }

    pub fn session_path(&self) -> &Path {
        self.session.path()
    }

    pub fn messages(&self) -> Vec<crate::session::Message> {
        self.session.messages()
    }
}
