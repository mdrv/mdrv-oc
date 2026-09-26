// ---------------------------------------------------------------------------
// commands/project.rs — `mdrv-oc project list|show` handlers.
// ---------------------------------------------------------------------------

use anyhow::{Context, Result};

use mdrv_oc as oc;

use super::util::Output;

pub(crate) fn cmd_project_list(db: &oc::Db, output: Output) -> Result<()> {
    let projects = oc::project::list(db).context("listing projects")?;
    output.emit(&projects, || {
        if projects.is_empty() {
            println!("(no projects)");
            return;
        }
        for p in &projects {
            let name = p.name.as_deref().unwrap_or("");
            println!("{}  {}  {}", p.id, p.worktree.display(), name);
        }
    });
    Ok(())
}

pub(crate) fn cmd_project_show(db: &oc::Db, id: &str, output: Output) -> Result<()> {
    let project = oc::project::get(db, id)
        .with_context(|| format!("looking up project {id:?}"))?
        .with_context(|| format!("no project with id {id:?}"))?;
    output.emit(&project, || {
        println!("id       : {}", project.id);
        println!("worktree : {}", project.worktree.display());
        if let Some(name) = &project.name {
            println!("name     : {name}");
        }
    });
    Ok(())
}
