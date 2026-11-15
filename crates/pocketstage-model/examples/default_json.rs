//! Print the factory-default project as JSON on stdout.
//!
//! Handy for two things: eyeballing what a fresh install boots into, and
//! seeding a library slot by hand (`... > ~/.pocketstage/projects/Idea.json`).
//! The arrangement itself lives in `pocketstage_model::demo` — this is just the
//! serialized view of `Project::default_project()`.
//!
//! ```sh
//! cargo run -q -p pocketstage-model --example default_json > /tmp/default.json
//! curl -X PUT localhost:7777/api/project -H 'Content-Type: application/json' \
//!     --data @/tmp/default.json
//! ```

use pocketstage_model::{Project, ProjectFile, SCHEMA_VERSION};

fn main() {
    let file = ProjectFile {
        version: SCHEMA_VERSION,
        project: Project::default_project(),
    };
    file.validate().expect("default project must validate");
    print!(
        "{}",
        file.to_json_pretty().expect("serialize default project")
    );
}
