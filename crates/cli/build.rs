// Shared from the workspace because Codixing currently ships workspace-built binaries.
#[path = "../../build-support/provenance.rs"]
mod provenance;

fn main() {
    provenance::emit();
}
