/// A reason.
#[derive(Debug, Clone)]
pub enum Reason {
    Same,
    Other,
}

const RAW: &str = r#"abc
def"#;

fn blk() { /* start
  end */ }

fn noted() { // TODO
}

fn plain() {
    // body
}
