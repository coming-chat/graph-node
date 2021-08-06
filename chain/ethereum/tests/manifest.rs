use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use graph::prelude::{
    anyhow, async_trait, tokio, DeploymentHash, Entity, Link, Logger, SubgraphManifest,
    SubgraphManifestValidationError, UnvalidatedSubgraphManifest,
};
use graph::{
    components::{
        link_resolver::{JsonValueStream, LinkResolver as LinkResolverTrait},
        store::EntityType,
    },
    data::subgraph::SubgraphFeature,
};

use graph_chain_ethereum::Chain;
use test_store::LOGGER;

#[derive(Default)]
struct TextResolver {
    texts: HashMap<String, String>,
}

impl TextResolver {
    fn add(&mut self, link: &str, text: &str) {
        self.texts.insert(link.to_owned(), text.to_owned());
    }
}

#[async_trait]
impl LinkResolverTrait for TextResolver {
    fn with_timeout(self, _timeout: Duration) -> Self {
        self
    }

    fn with_retries(self) -> Self {
        self
    }

    async fn cat(&self, _logger: &Logger, link: &Link) -> Result<Vec<u8>, anyhow::Error> {
        self.texts
            .get(&link.link)
            .ok_or(anyhow!("No text for {}", &link.link))
            .map(|text| text.to_owned().into_bytes())
    }

    async fn json_stream(
        &self,
        _logger: &Logger,
        _link: &Link,
    ) -> Result<JsonValueStream, anyhow::Error> {
        unimplemented!()
    }
}

const GQL_SCHEMA: &str = "type Thing @entity { id: ID! }";

const ABI: &str = "[{\"type\":\"function\", \"inputs\": [{\"name\": \"i\",\"type\": \"uint256\"}],\"name\":\"get\",\"outputs\": [{\"type\": \"address\",\"name\": \"o\"}]}]";

const MAPPING: &str = "export function handleGet(call: getCall): void {}";

async fn resolve_manifest(text: &str) -> SubgraphManifest<graph_chain_ethereum::Chain> {
    let mut resolver = TextResolver::default();
    let id = DeploymentHash::new("Qmmanifest").unwrap();

    resolver.add(id.as_str(), text);
    resolver.add("/ipfs/Qmschema", GQL_SCHEMA);
    resolver.add("/ipfs/Qmabi", ABI);
    resolver.add("/ipfs/Qmmapping", MAPPING);

    SubgraphManifest::resolve(id, &resolver, &LOGGER)
        .await
        .expect("Parsing simple manifest works")
}

async fn resolve_unvalidated(text: &str) -> UnvalidatedSubgraphManifest<Chain> {
    let mut resolver = TextResolver::default();
    let id = DeploymentHash::new("Qmmanifest").unwrap();

    resolver.add(id.as_str(), text);
    resolver.add("/ipfs/Qmschema", GQL_SCHEMA);

    UnvalidatedSubgraphManifest::resolve(id, Arc::new(resolver), &LOGGER)
        .await
        .expect("Parsing simple manifest works")
}

// Some of these manifest tests should be made chain-independent, but for
// now we just run them for the ethereum `Chain`

#[tokio::test]
async fn simple_manifest() {
    const YAML: &str = "
dataSources: []
schema:
  file:
    /: /ipfs/Qmschema
specVersion: 0.0.2
";

    let manifest = resolve_manifest(YAML).await;

    assert_eq!("Qmmanifest", manifest.id.as_str());
    assert!(manifest.graft.is_none());
}

#[tokio::test]
async fn graft_manifest() {
    const YAML: &str = "
dataSources: []
schema:
  file:
    /: /ipfs/Qmschema
graft:
  base: Qmbase
  block: 12345
specVersion: 0.0.2
";

    let manifest = resolve_manifest(YAML).await;

    assert_eq!("Qmmanifest", manifest.id.as_str());
    let graft = manifest.graft.expect("The manifest has a graft base");
    assert_eq!("Qmbase", graft.base.as_str());
    assert_eq!(12345, graft.block);
}

#[test]
fn graft_invalid_manifest() {
    const YAML: &str = "
dataSources: []
schema:
  file:
    /: /ipfs/Qmschema
graft:
  base: Qmbase
  block: 1
specVersion: 0.0.2
";

    test_store::run_test_sequentially(|store| async move {
        let store = store.subgraph_store();

        let unvalidated = resolve_unvalidated(YAML).await;
        let subgraph = DeploymentHash::new("Qmbase").unwrap();

        //
        // Validation against subgraph that hasn't synced anything fails
        //
        let deployment = test_store::create_test_subgraph(&subgraph, GQL_SCHEMA);
        // This check is awkward since the test manifest has other problems
        // that the validation complains about as setting up a valid manifest
        // would be a bit more work; we just want to make sure that
        // graft-related checks work
        let msg = unvalidated
            .validate(store.clone())
            .expect_err("Validation must fail")
            .into_iter()
            .find(|e| matches!(e, SubgraphManifestValidationError::GraftBaseInvalid(_)))
            .expect("There must be a GraftBaseInvalid error")
            .to_string();
        assert_eq!(
            "the graft base is invalid: failed to graft onto `Qmbase` since \
            it has not processed any blocks",
            msg
        );

        let mut thing = Entity::new();
        thing.set("id", "datthing");
        test_store::insert_entities(&deployment, vec![(EntityType::from("Thing"), thing)])
            .expect("Can insert a thing");

        // Validation against subgraph that has not reached the graft point fails
        let unvalidated = resolve_unvalidated(YAML).await;
        let msg = unvalidated
            .validate(store)
            .expect_err("Validation must fail")
            .into_iter()
            .find(|e| matches!(e, SubgraphManifestValidationError::GraftBaseInvalid(_)))
            .expect("There must be a GraftBaseInvalid error")
            .to_string();
        assert_eq!(
            "the graft base is invalid: failed to graft onto `Qmbase` \
            at block 1 since it has only processed block 0",
            msg
        );
    })
}

#[tokio::test]
async fn parse_call_handlers() {
    const YAML: &str = "
dataSources:
  - kind: ethereum/contract
    name: Factory
    network: mainnet
    source:
      abi: Factory
      startBlock: 9562480
    mapping:
      kind: ethereum/events
      apiVersion: 0.0.4
      language: wasm/assemblyscript
      entities:
        - TestEntity
      file:
        /: /ipfs/Qmmapping
      abis:
        - name: Factory
          file:
            /: /ipfs/Qmabi
      callHandlers:
        - function: get(address)
          handler: handleget
schema:
  file:
    /: /ipfs/Qmschema
specVersion: 0.0.2
";

    let manifest = resolve_manifest(YAML).await;
    let requires_traces = manifest.requires_traces();

    assert_eq!("Qmmanifest", manifest.id.as_str());
    assert_eq!(true, requires_traces);
}

#[test]
fn undeclared_grafting_feature_causes_feature_validation_error() {
    const YAML: &str = "
specVersion: 0.0.4
dataSources: []
schema:
  file:
    /: /ipfs/Qmschema
graft:
  base: Qmbase
  block: 1
";
    test_store::run_test_sequentially(|store| async move {
        let store = store.subgraph_store();
        let unvalidated = resolve_unvalidated(YAML).await;
        let error_msg = unvalidated
            .validate(store.clone())
            .expect_err("Validation must fail")
            .into_iter()
            .find(|e| {
                matches!(
                    e,
                    SubgraphManifestValidationError::FeatureValidationError(_)
                )
            })
            .expect("There must be a FeatureValidation error")
            .to_string();
        assert_eq!(
            "The feature `grafting` is used by the subgraph but it is not declared in the manifest.",
            error_msg
        )
    })
}

#[test]
fn declared_grafting_feature_causes_no_feature_validation_errors() {
    const YAML: &str = "
specVersion: 0.0.4
features:
  - grafting
dataSources: []
schema:
  file:
    /: /ipfs/Qmschema
graft:
  base: Qmbase
  block: 1
";
    test_store::run_test_sequentially(|store| async move {
        let store = store.subgraph_store();
        let unvalidated = resolve_unvalidated(YAML).await;
        assert!(unvalidated
            .validate(store.clone())
            .expect_err("Validation must fail")
            .into_iter()
            .find(|e| {
                matches!(
                    e,
                    SubgraphManifestValidationError::FeatureValidationError(_)
                )
            })
            .is_none());
        let manifest = resolve_manifest(YAML).await;
        assert!(manifest.features.contains(&SubgraphFeature::Grafting))
    })
}

#[test]
fn declared_non_fatal_errors_feature_causes_no_feature_validation_errors() {
    const YAML: &str = "
specVersion: 0.0.4
features:
  - nonFatalErrors
dataSources: []
schema:
  file:
    /: /ipfs/Qmschema
";
    test_store::run_test_sequentially(|store| async move {
        let store = store.subgraph_store();
        let unvalidated = resolve_unvalidated(YAML).await;
        assert!(unvalidated
            .validate(store.clone())
            .expect_err("Validation must fail")
            .into_iter()
            .find(|e| {
                matches!(
                    e,
                    SubgraphManifestValidationError::FeatureValidationError(_)
                )
            })
            .is_none());

        let manifest = resolve_manifest(YAML).await;
        assert!(manifest.features.contains(&SubgraphFeature::NonFatalErrors))
    });
}
