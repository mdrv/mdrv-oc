// ---------------------------------------------------------------------------
// commands/mod.rs — handler modules for the CLI.
//
// `src/cli.rs` owns the clap definitions and the dispatch; each module here
// holds the handlers for one command family. `util` carries everything those
// handlers share (output mode, DB opening, prompts, formatting).
// ---------------------------------------------------------------------------

mod project;
mod session;
pub(crate) mod util;

pub(crate) use project::{cmd_project_list, cmd_project_show};
pub(crate) use session::{
    cmd_session_export, cmd_session_import, cmd_session_inspect, cmd_session_list,
    cmd_session_move, cmd_session_show, cmd_session_sync,
};
pub(crate) use util::{open_db, Output};
