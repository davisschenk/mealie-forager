use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::config::Config;

/// Turns the model's output into the schema.org JSON-LD Mealie's scraper ingests.
pub fn to_json_ld(recipe: &Value, url: &str, image: Option<&str>, tags: &[String]) -> Value {
    let mut keywords: Vec<String> = Vec::new();
    let supplied = recipe["keywords"].as_array().into_iter().flatten();
    for k in supplied
        .filter_map(Value::as_str)
        .map(str::to_string)
        .chain(tags.iter().cloned())
    {
        let k = k.trim().to_string();
        if !k.is_empty() && !keywords.iter().any(|e| e.eq_ignore_ascii_case(&k)) {
            keywords.push(k);
        }
    }

    let mut ld = Map::new();
    ld.insert("@context".into(), json!("https://schema.org"));
    ld.insert("@type".into(), json!("Recipe"));
    ld.insert("url".into(), json!(url));
    if let Some(image) = image {
        ld.insert("image".into(), json!(image));
    }
    for key in [
        "name",
        "description",
        "recipeYield",
        "prepTime",
        "cookTime",
        "totalTime",
    ] {
        if let Some(v) = recipe[key].as_str().filter(|v| !v.trim().is_empty()) {
            ld.insert(key.into(), json!(v));
        }
    }
    ld.insert(
        "recipeIngredient".into(),
        recipe["recipeIngredient"].clone(),
    );
    let steps: Vec<Value> = recipe["recipeInstructions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["text"].as_str())
        .map(|text| json!({ "@type": "HowToStep", "text": text }))
        .collect();
    ld.insert("recipeInstructions".into(), json!(steps));
    if !keywords.is_empty() {
        ld.insert("keywords".into(), json!(keywords.join(", ")));
    }
    if let Some(n) = recipe["nutrition"].as_object() {
        let mut nutrition: Map<String, Value> = n
            .iter()
            .filter(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !nutrition.is_empty() {
            nutrition.insert("@type".into(), json!("NutritionInformation"));
            ld.insert("nutrition".into(), Value::Object(nutrition));
        }
    }
    Value::Object(ld)
}

pub struct Mealie<'a> {
    pub http: &'a reqwest::Client,
    pub config: &'a Config,
}

impl Mealie<'_> {
    pub async fn create_from_json_ld(&self, ld: &Value, url: &str) -> Result<String> {
        let resp = self
            .http
            .post(format!(
                "{}/api/recipes/create/html-or-json",
                self.config.mealie_url
            ))
            .bearer_auth(&self.config.mealie_api_key)
            .json(&json!({ "data": ld.to_string(), "url": url, "includeTags": true }))
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .context("could not reach Mealie")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["detail"]["message"].as_str().map(str::to_string))
                .unwrap_or(body);
            bail!(
                "Mealie returned {status}: {}",
                detail.chars().take(400).collect::<String>()
            );
        }
        serde_json::from_str::<String>(&body).context("Mealie did not return a recipe slug")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_json_ld_and_merges_tags() {
        let recipe = json!({
            "is_recipe": true,
            "name": "Garlic noodles",
            "description": "Buttery and quick.",
            "recipeYield": "2 servings",
            "prepTime": null,
            "cookTime": "PT10M",
            "totalTime": "",
            "recipeIngredient": ["200 g noodles", "4 cloves garlic"],
            "recipeInstructions": [{"text": "Boil noodles."}, {"text": "Toss with garlic butter."}],
            "keywords": ["noodles", "Quick"],
            "nutrition": {"calories": "450 kcal", "proteinContent": null, "fatContent": null, "carbohydrateContent": null}
        });
        let tags = vec!["quick".to_string(), "weeknight".to_string()];
        let ld = to_json_ld(&recipe, "https://x/1", Some("https://img"), &tags);

        assert_eq!(ld["@type"], "Recipe");
        assert_eq!(ld["image"], "https://img");
        assert_eq!(ld["cookTime"], "PT10M");
        assert!(ld.get("prepTime").is_none());
        assert!(ld.get("totalTime").is_none());
        assert!(ld.get("is_recipe").is_none());
        assert_eq!(ld["keywords"], "noodles, Quick, weeknight");
        assert_eq!(ld["recipeInstructions"][1]["@type"], "HowToStep");
        assert_eq!(
            ld["nutrition"],
            json!({"@type": "NutritionInformation", "calories": "450 kcal"})
        );
    }
}
