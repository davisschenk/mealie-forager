use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::config::Config;

/// Turns the model's output into the schema.org JSON-LD Mealie's scraper ingests.
/// Only the job's own tags become keywords (and so Mealie tags); the model's
/// keywords would clutter the tag list, and categories cover what they describe.
pub fn to_json_ld(recipe: &Value, url: &str, image: Option<&str>, tags: &[String]) -> Value {
    let mut keywords: Vec<String> = Vec::new();
    for k in tags {
        let k = k.trim().to_string();
        if !k.is_empty() && !keywords.iter().any(|e| e.eq_ignore_ascii_case(&k)) {
            keywords.push(k);
        }
    }

    let mut ld = Map::new();
    ld.insert("@context".into(), json!("https://schema.org"));
    ld.insert("@type".into(), json!("Recipe"));
    if !url.is_empty() {
        ld.insert("url".into(), json!(url));
    }
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
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.mealie_url)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Option<Value>> {
        let resp = req
            .bearer_auth(&self.config.mealie_api_key)
            .send()
            .await
            .context("could not reach Mealie")?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let parsed = serde_json::from_str::<Value>(&body).ok();
            let detail = parsed
                .as_ref()
                .and_then(|v| {
                    v["detail"]["message"]
                        .as_str()
                        .or_else(|| v["detail"].as_str())
                        .map(str::to_string)
                })
                .or_else(|| parsed.map(|v| v["detail"].to_string()))
                .unwrap_or(body);
            bail!(
                "Mealie returned {status}: {}",
                detail.chars().take(400).collect::<String>()
            );
        }
        if body.trim().is_empty() {
            return Ok(Some(Value::Null));
        }
        Ok(Some(
            serde_json::from_str(&body).context("Mealie returned invalid JSON")?,
        ))
    }

    async fn call(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        self.send(req)
            .await?
            .with_context(|| format!("Mealie returned 404 for {what}"))
    }

    async fn slug(&self, req: reqwest::RequestBuilder) -> Result<String> {
        let value = self.call(req, "recipe creation").await?;
        value
            .as_str()
            .map(str::to_string)
            .context("Mealie did not return a recipe slug")
    }

    pub async fn create_from_json_ld(&self, ld: &Value, url: &str) -> Result<String> {
        self.slug(
            self.http
                .post(self.url("/api/recipes/create/html-or-json"))
                .json(&if url.is_empty() {
                    json!({ "data": ld.to_string(), "includeTags": true })
                } else {
                    json!({ "data": ld.to_string(), "url": url, "includeTags": true })
                })
                .timeout(Duration::from_secs(180)),
        )
        .await
    }

    /// Lets Mealie's own scraper import a recipe web page. Sites' SEO keywords
    /// aren't imported as tags; the job's tags are added afterwards.
    pub async fn create_from_url(&self, url: &str) -> Result<String> {
        self.slug(
            self.http
                .post(self.url("/api/recipes/create/url"))
                .json(&json!({ "url": url, "includeTags": false }))
                .timeout(Duration::from_secs(180)),
        )
        .await
    }

    /// Mealie's AI import (3.28+) from photos and/or text; the first image
    /// becomes the cover.
    pub async fn create_with_ai(
        &self,
        content: Option<String>,
        images: Vec<(String, &'static str, Vec<u8>)>,
    ) -> Result<String> {
        let mut form = reqwest::multipart::Form::new();
        if let Some(content) = content {
            form = form.text("content", content);
        }
        for (name, mime, bytes) in images {
            form = form.part(
                "images",
                reqwest::multipart::Part::bytes(bytes)
                    .file_name(name)
                    .mime_str(mime)?,
            );
        }
        self.slug(
            self.http
                .post(self.url("/api/recipes/create/ai"))
                .multipart(form)
                .timeout(Duration::from_secs(300)),
        )
        .await
        .context(
            "Mealie's AI import failed (needs Mealie 3.28+ with OpenAI and image services enabled)",
        )
    }

    /// Imports a recipe exported from Mealie as a .zip.
    pub async fn create_from_zip(&self, name: String, bytes: Vec<u8>) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name)
            .mime_str("application/zip")?;
        self.slug(
            self.http
                .post(self.url("/api/recipes/create/zip"))
                .multipart(reqwest::multipart::Form::new().part("archive", part))
                .timeout(Duration::from_secs(180)),
        )
        .await
    }

    pub async fn recipe(&self, slug: &str) -> Result<Option<Value>> {
        self.send(self.http.get(self.url(&format!("/api/recipes/{slug}"))))
            .await
    }

    /// Deletes a recipe; one that is already gone counts as deleted.
    pub async fn delete_recipe(&self, slug: &str) -> Result<()> {
        self.send(self.http.delete(self.url(&format!("/api/recipes/{slug}"))))
            .await?;
        Ok(())
    }

    /// Replaces the whole recipe; returns it as saved (the slug follows the name).
    pub async fn update_recipe(&self, slug: &str, recipe: &Value) -> Result<Value> {
        self.call(
            self.http
                .put(self.url(&format!("/api/recipes/{slug}")))
                .json(recipe),
            "recipe update",
        )
        .await
    }

    async fn items(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<Value>> {
        let page = self
            .call(self.http.get(self.url(path)).query(query), path)
            .await?;
        Ok(page["items"].as_array().cloned().unwrap_or_default())
    }

    /// Every recipe in the group (summaries, which include tags).
    pub async fn recipes(&self) -> Result<Vec<Value>> {
        self.items("/api/recipes", &[("perPage", "-1"), ("orderBy", "name")])
            .await
    }

    pub async fn units(&self) -> Result<Vec<Value>> {
        self.items("/api/units", &[("perPage", "-1")]).await
    }

    pub async fn categories(&self) -> Result<Vec<Value>> {
        self.items("/api/organizers/categories", &[("perPage", "-1")])
            .await
    }

    pub async fn search_foods(&self, search: &str) -> Result<Vec<Value>> {
        self.items("/api/foods", &[("search", search), ("perPage", "15")])
            .await
    }

    pub async fn create_food(&self, name: &str, plural_name: &str) -> Result<Value> {
        self.call(
            self.http
                .post(self.url("/api/foods"))
                .json(&json!({ "name": name, "pluralName": plural_name, "description": "" })),
            "food creation",
        )
        .await
    }

    pub async fn create_unit(
        &self,
        name: &str,
        plural_name: &str,
        abbreviation: &str,
    ) -> Result<Value> {
        self.call(
            self.http.post(self.url("/api/units")).json(&json!({
                "name": name,
                "pluralName": plural_name,
                "abbreviation": abbreviation,
                "description": "",
                "fraction": true,
                "useAbbreviation": false,
            })),
            "unit creation",
        )
        .await
    }

    /// Finds a tag by name (ignoring case), creating it when missing.
    pub async fn ensure_tag(&self, name: &str) -> Result<Value> {
        let tags = self
            .items(
                "/api/organizers/tags",
                &[("search", name), ("perPage", "-1")],
            )
            .await?;
        if let Some(tag) = tags.into_iter().find(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        }) {
            return Ok(tag);
        }
        self.call(
            self.http
                .post(self.url("/api/organizers/tags"))
                .json(&json!({ "name": name })),
            "tag creation",
        )
        .await
    }

    /// Deletes unused tags that came from hashtags; returns their names.
    pub async fn delete_empty_hashtags(&self) -> Result<Vec<String>> {
        let empty = self
            .call(
                self.http.get(self.url("/api/organizers/tags/empty")),
                "empty tags",
            )
            .await?;
        let mut deleted = Vec::new();
        for tag in empty.as_array().into_iter().flatten() {
            let (Some(id), Some(name)) = (tag["id"].as_str(), tag["name"].as_str()) else {
                continue;
            };
            if name.starts_with('#') {
                self.send(
                    self.http
                        .delete(self.url(&format!("/api/organizers/tags/{id}"))),
                )
                .await?;
                deleted.push(name.to_string());
            }
        }
        Ok(deleted)
    }
}

/// Adds `add` (by name, ignoring case) to a recipe's tags, optionally dropping
/// tags that came from hashtags.
pub fn merge_tags(recipe: &mut Value, add: &[Value], drop_hashtags: bool) {
    let mut tags: Vec<Value> = recipe["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| !drop_hashtags || !t["name"].as_str().unwrap_or_default().starts_with('#'))
        .cloned()
        .collect();
    for tag in add {
        let name = tag["name"].as_str().unwrap_or_default();
        if !tags.iter().any(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        }) {
            tags.push(tag.clone());
        }
    }
    recipe["tags"] = Value::Array(tags);
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
        assert_eq!(ld["keywords"], "quick, weeknight", "only the job's tags");
        assert_eq!(ld["recipeInstructions"][1]["@type"], "HowToStep");
        assert_eq!(
            ld["nutrition"],
            json!({"@type": "NutritionInformation", "calories": "450 kcal"})
        );
    }

    #[test]
    fn merge_tags_drops_hashtags_and_dedupes() {
        let mut recipe = json!({ "tags": [
            { "id": "1", "name": "#foodtok" },
            { "id": "2", "name": "Imported" },
        ]});
        merge_tags(
            &mut recipe,
            &[
                json!({ "id": "2", "name": "imported" }),
                json!({ "id": "3", "name": "Imported Clean" }),
            ],
            true,
        );
        let names: Vec<&str> = recipe["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Imported", "Imported Clean"]);
    }
}
