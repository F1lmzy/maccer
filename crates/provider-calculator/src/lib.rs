use anyhow::{Result, bail};
use launcher_core::{
    Action, ActionId, ActionOutcome, IconDescriptor, Item, ItemId, Provider, ProviderId,
    SearchContext, SearchQuery,
};
use launcher_macos::Platform;
use serde_json::json;
use std::sync::Arc;

const ID: &str = "calculator";
const COPY: &str = "copy";

pub struct CalculatorProvider {
    platform: Arc<dyn Platform>,
}
impl CalculatorProvider {
    pub fn new(platform: Arc<dyn Platform>) -> Self {
        Self { platform }
    }
}
impl Provider for CalculatorProvider {
    fn id(&self) -> ProviderId {
        ProviderId(ID.into())
    }
    fn name(&self) -> &str {
        "Calculator"
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() || ctx.limit == 0 {
            return Ok(Vec::new());
        }
        let expression = query.text.trim();
        if expression.is_empty() {
            return Ok(Vec::new());
        }
        let value = meval::eval_str(expression)
            .map_err(|e| anyhow::anyhow!("invalid calculator expression: {e}"))?;
        if !value.is_finite() {
            bail!("calculator result must be finite");
        }
        let rounded = value.round();
        let result = if (value - rounded).abs() <= f64::EPSILON * value.abs().max(1.0) * 4.0
            && rounded.abs() < 1e15
        {
            format!("{rounded:.0}")
        } else {
            value.to_string()
        };
        Ok(vec![Item {
            id: ItemId(format!("calculator:{result}")),
            provider: ProviderId(ID.into()),
            title: result.clone(),
            subtitle: Some("Calculator".into()),
            keywords: vec![expression.into()],
            icon: Some(IconDescriptor::Text("=".into())),
            score: 0.0,
            payload: json!({"result":result,"expression":expression}),
        }])
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if item.provider == ProviderId(ID.into())
            && item
                .payload
                .get("result")
                .and_then(|v| v.as_str())
                .is_some()
        {
            vec![Action {
                id: ActionId(COPY.into()),
                title: "Copy Result".into(),
            }]
        } else {
            Vec::new()
        }
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if item.provider != ProviderId(ID.into()) {
            bail!("item is not a calculator result");
        }
        if action.id.0 != COPY {
            bail!("unsupported calculator action: {}", action.id.0);
        }
        let result = item
            .payload
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("calculator result is missing"))?;
        self.platform.copy_text(result)?;
        Ok(ActionOutcome::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::Mutex};
    #[derive(Default)]
    struct Fake(Mutex<Vec<String>>);
    impl Platform for Fake {
        fn launch_application(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn reveal_in_finder(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn open_url(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn copy_text(&self, s: &str) -> Result<()> {
            self.0.lock().unwrap().push(s.into());
            Ok(())
        }
    }
    fn provider() -> (CalculatorProvider, Arc<Fake>) {
        let f = Arc::new(Fake::default());
        (CalculatorProvider::new(f.clone()), f)
    }
    fn query(s: &str) -> SearchQuery {
        SearchQuery {
            raw: s.into(),
            text: s.into(),
        }
    }
    fn ctx() -> SearchContext {
        SearchContext {
            generation: 1,
            cancellation: Default::default(),
            limit: 10,
        }
    }
    #[test]
    fn evaluates_arithmetic_and_sqrt() {
        let (p, _) = provider();
        for (e, want) in [("2+2", "4"), ("100*1.15", "115"), ("sqrt(144)", "12")] {
            assert_eq!(p.search(&query(e), &ctx()).unwrap()[0].title, want);
        }
    }
    #[test]
    fn rejects_invalid_expressions_and_non_finite_values() {
        let (p, _) = provider();
        assert!(
            p.search(&query("not code"), &ctx())
                .unwrap_err()
                .to_string()
                .contains("invalid calculator expression")
        );
        assert!(
            p.search(&query("1/0"), &ctx())
                .unwrap_err()
                .to_string()
                .contains("finite")
        );
    }
    #[test]
    fn copies_result_only_for_valid_copy_action() {
        let (p, f) = provider();
        let item = p.search(&query("2+2"), &ctx()).unwrap().remove(0);
        assert!(
            p.activate(
                &item,
                &Action {
                    id: ActionId("run".into()),
                    title: "Run".into()
                }
            )
            .is_err()
        );
        p.activate(&item, &p.actions(&item)[0]).unwrap();
        assert_eq!(f.0.lock().unwrap().as_slice(), ["4"]);
    }
}
