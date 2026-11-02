// ---------------------------------------------------------------------------
// commands/mod.rs — handler modules for the CLI.
//
// `src/cli.rs` owns the clap definitions and the dispatch; each module here
// holds the handlers for one command family. `util` carries everything those
// handlers share (output mode, DB opening, prompts, formatting).
// ---------------------------------------------------------------------------

mod project;
mod prompt;
mod session;
pub(crate) mod util;

pub(crate) use project::{cmd_project_list, cmd_project_show};
pub(crate) use prompt::{
    PromptArgs, cmd_daemon, cmd_prompt_cancel, cmd_prompt_list, cmd_session_list_server,
    cmd_session_prompt,
};
pub(crate) use session::{
    cmd_session_export, cmd_session_import, cmd_session_inspect, cmd_session_list,
    cmd_session_move, cmd_session_show, cmd_session_sync,
};
pub(crate) use util::{Output, open_db};
