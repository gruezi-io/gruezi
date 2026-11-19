//! The release workflow read as data: jobs, their `needs` graph, matrix rows and
//! steps. Only what the tests assert on and the runner executes is modelled; the
//! workflow's own shell makes every decision.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer, de::SeqAccess, de::Visitor};
use std::{collections::BTreeMap, fmt};

#[derive(Deserialize)]
pub struct Workflow {
    pub name: String,
    #[serde(rename = "on")]
    pub triggers: BTreeMap<String, Dispatch>,
    pub concurrency: Option<Concurrency>,
    #[serde(default)]
    pub jobs: BTreeMap<String, Job>,
}

#[derive(Deserialize)]
pub struct Dispatch {
    #[serde(default)]
    pub inputs: BTreeMap<String, Input>,
}

#[derive(Deserialize)]
pub struct Input {
    #[serde(default)]
    pub required: bool,
}

#[derive(Deserialize)]
pub struct Concurrency {
    pub group: String,
    #[serde(default, rename = "cancel-in-progress")]
    pub cancel_in_progress: bool,
    #[serde(default)]
    pub queue: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct Job {
    #[serde(default, deserialize_with = "names")]
    pub needs: Vec<String>,
    #[serde(default, rename = "runs-on")]
    pub runs_on: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
    pub environment: Option<String>,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    pub strategy: Option<Strategy>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Deserialize, Clone)]
pub struct Strategy {
    pub matrix: Option<Matrix>,
}

#[derive(Deserialize, Clone)]
pub struct Matrix {
    #[serde(default)]
    pub include: Vec<BTreeMap<String, String>>,
}

#[derive(Deserialize, Clone)]
pub struct Step {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default, rename = "if")]
    pub condition: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub with: BTreeMap<String, Scalar>,
}

/// A `with:` value; YAML scalars arrive as text, numbers or booleans.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Scalar {
    Text(String),
    Number(i64),
    Flag(bool),
}

impl Scalar {
    pub fn text(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::Number(value) => value.to_string(),
            Self::Flag(value) => value.to_string(),
        }
    }
}

/// `needs` is one name or a list of them.
fn names<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Names;

    impl<'de> Visitor<'de> for Names {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a job name or a list of job names")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![value.to_owned()])
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut names = Vec::new();
            while let Some(name) = sequence.next_element()? {
                names.push(name);
            }
            Ok(names)
        }
    }

    deserializer.deserialize_any(Names)
}

/// A local composite action: its shell steps and declared outputs.
#[derive(Deserialize)]
pub struct Action {
    #[serde(default)]
    pub runs: ActionRuns,
}

#[derive(Deserialize, Default)]
pub struct ActionRuns {
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// Parse the release workflow of the repository under test.
pub fn workflow() -> Result<Workflow> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(super::world::WORKFLOW);
    let text = std::fs::read_to_string(&path)?;
    serde_saphyr::from_str(&text)
        .map_err(|error| anyhow!("cannot parse {}: {error}", path.display()))
}
