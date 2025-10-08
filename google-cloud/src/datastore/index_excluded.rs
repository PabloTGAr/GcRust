use crate::datastore::Error;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, fs::File, path::Path};

/// Information for the exclusion of indexes in Datastore
/// Example: "my_project/index_excluded.yaml"
///
/// kind:
///   customer:
///     property:
///     - email
///     - lastName
///
/// By default all the fields are "false", it is only necessary to include
/// the fields that we want to exclude
///
/// To find the file you have to create an environment variable with the name "INDEX_EXCLUDED"
/// and the path of the file
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexExcluded {
    ///
    pub kind: HashMap<String, PropertyExcluded>,
}

///
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyExcluded {
    ///
    pub property: Vec<String>,
}

impl IndexExcluded {
    /// Open and deserialize the file with the configuration for the exclusion of indexes in Datastore
    pub(crate) fn new() -> Result<IndexExcluded, Error> {
        let path = match env::var("INDEX_EXCLUDED") {
            Ok(env) => env,
            Err(_) => return Ok(IndexExcluded { kind: HashMap::new() }),
        };

        let path = Path::new(&path);
        let file = match File::open(path) {
            Ok(file) => file,
            Err(_) => return Ok(IndexExcluded { kind: HashMap::new() }),
        };
        let deserialized_yaml = serde_yaml::from_reader(file)?;

        Ok(deserialized_yaml)
    }

    ///
    pub(crate) fn ckeck_value(self, kind: String, property: String) -> Vec<String> {
        let property_excluded = match self.kind.get(&kind) {
            Some(p) => p.to_owned(),
            None => return vec![],
        };

        property_excluded
            .property
            .into_iter()
            .filter_map(|element| {
                element
                    .split_once(".")
                    .or(Some((element.as_str(), "")))
                    .filter(|(first, _)| first.to_string() == property)
                    .map(|(_, rest)| rest.to_string())
            })
            .collect::<Vec<String>>()
    }
}
