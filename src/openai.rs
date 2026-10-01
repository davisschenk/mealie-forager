use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

use crate::config::Config;

const SYSTEM_PROMPT: &str = "You turn social-media cooking posts into clean recipes.\n\
You receive the post caption, a transcript of the spoken audio, and sometimes images.\n\
- Set is_recipe to false (with a short reason) if there is no dish that can be cooked from the content.\n\
- A recipe needs both its ingredients and its method from the content. Ingredients named only in the spoken steps count, and so do steps that are only shown or narrated. If the ingredients or the method are missing (only the finished dish is shown, only ingredients are listed, \"recipe in bio\", a teaser for another post), set is_recipe to false and say which part is missing. Never invent a missing ingredient list or method; only quantities may be estimated.\n\
- recipe_names lists the name of every separate dish the content gives a recipe for. Components of one dish (a sauce, dough, marinade, topping or the side it is served with) are part of that dish, not separate recipes. If the user instructions pick one dish, extract only that one and list only it. If there are several separate dishes, list them all and fill the other fields with the first one.\n\
- Merge caption and transcript; prefer explicit quantities from either. When quantities are missing, give reasonable estimates and mark them with \"(approx.)\".\n\
- Write each ingredient as one line: quantity, unit, ingredient, preparation (e.g. \"2 tbsp olive oil\", \"1 onion, finely diced\").\n\
- Write short, imperative instruction steps in cooking order. Do not reference the video, creator or social platform.\n\
- The description is one or two appetising sentences about the dish, not about the post.\n\
- Durations are ISO 8601 (e.g. PT25M) or null when unknown; recipeYield like \"4 servings\" or null.\n\
- Keywords are short lowercase tags (cuisine, main ingredient, meal type); always include every supplied keyword verbatim.\n\
- Never invent nutrition values; use null unless the content states them.\n\
- Write in the language of the source content.";

fn nullable_string() -> Value {
    json!({ "type": ["string", "null"] })
}

pub fn recipe_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "is_recipe", "not_recipe_reason", "recipe_names", "name", "description", "recipeYield",
            "prepTime", "cookTime", "totalTime", "recipeIngredient", "recipeInstructions",
            "keywords", "nutrition"
        ],
        "properties": {
            "is_recipe": { "type": "boolean" },
            "not_recipe_reason": nullable_string(),
            "recipe_names": { "type": "array", "items": { "type": "string" } },
            "name": { "type": "string" },
            "description": { "type": "string" },
            "recipeYield": nullable_string(),
            "prepTime": nullable_string(),
            "cookTime": nullable_string(),
            "totalTime": nullable_string(),
            "recipeIngredient": { "type": "array", "items": { "type": "string" } },
            "recipeInstructions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text"],
                    "properties": { "text": { "type": "string" } }
                }
            },
            "keywords": { "type": "array", "items": { "type": "string" } },
            "nutrition": {
                "anyOf": [
                    { "type": "null" },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["calories", "proteinContent", "fatContent", "carbohydrateContent"],
                        "properties": {
                            "calories": nullable_string(),
                            "proteinContent": nullable_string(),
                            "fatContent": nullable_string(),
                            "carbohydrateContent": nullable_string()
                        }
                    }
                ]
            }
        }
    })
}

/// Rejects an extraction that isn't exactly one complete recipe; `source` names
/// what was read ("post", "recording") for the error message.
pub fn check_recipe(recipe: &Value, source: &str) -> Result<()> {
    if recipe["is_recipe"] == false {
        let reason = recipe["not_recipe_reason"]
            .as_str()
            .unwrap_or("no reason given");
        bail!("no recipe found in this {source}: {reason}");
    }
    let names: Vec<&str> = recipe["recipe_names"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::trim).filter(|n| !n.is_empty()))
        .collect();
    if names.len() > 1 {
        bail!(
            "this {source} has {} separate recipes ({}); Forager imports one recipe per job, \
             so submit it again with a note naming the one you want",
            names.len(),
            names.join(", ")
        );
    }
    let filled = |field: &str, key: Option<&str>| {
        recipe[field].as_array().into_iter().flatten().any(|v| {
            key.map_or(v, |k| &v[k])
                .as_str()
                .is_some_and(|t| !t.trim().is_empty())
        })
    };
    let mut missing = Vec::new();
    if !filled("recipeIngredient", None) {
        missing.push("ingredients");
    }
    if !filled("recipeInstructions", Some("text")) {
        missing.push("instructions");
    }
    if !missing.is_empty() {
        bail!(
            "the recipe in this {source} has no {}, so it wasn't imported",
            missing.join(" or ")
        );
    }
    Ok(())
}

pub struct ExtractInput<'a> {
    pub url: &'a str,
    pub title: Option<&'a str>,
    pub uploader: Option<&'a str>,
    pub description: Option<&'a str>,
    pub transcript: Option<&'a str>,
    pub tags: &'a [String],
    pub note: Option<&'a str>,
    pub images: &'a [String],
}

pub struct Completion {
    pub value: Value,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

pub fn user_prompt(input: &ExtractInput, extra: Option<&str>) -> String {
    let mut prompt = format!("<post url=\"{}\">\n", input.url);
    if let Some(t) = input.title {
        prompt += &format!("<title>{t}</title>\n");
    }
    if let Some(u) = input.uploader {
        prompt += &format!("<creator>{u}</creator>\n");
    }
    prompt += &format!(
        "<caption>\n{}\n</caption>\n",
        input.description.unwrap_or("(no caption)")
    );
    prompt += &format!(
        "<transcript>\n{}\n</transcript>\n</post>\n",
        input.transcript.unwrap_or("(no spoken audio)")
    );
    if !input.tags.is_empty() {
        prompt += &format!("<keywords>{}</keywords>\n", input.tags.join(", "));
    }
    for instructions in [extra, input.note].into_iter().flatten() {
        prompt += &format!("<user_instructions>{instructions}</user_instructions>\n");
    }
    prompt
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
    refusal: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
}

pub struct OpenAi<'a> {
    pub http: &'a reqwest::Client,
    pub config: &'a Config,
}

async fn error_body(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or(body);
    format!(
        "{status}: {}",
        message.chars().take(500).collect::<String>()
    )
}

impl OpenAi<'_> {
    pub async fn transcribe(&self, audio: &Path) -> Result<String> {
        let bytes = tokio::fs::read(audio).await?;
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name("speech.mp3")
            .mime_str("audio/mpeg")?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.config.transcription_model.clone())
            .text("response_format", "json")
            .part("file", part);
        let resp = self
            .http
            .post(format!("{}/audio/transcriptions", self.config.openai_url))
            .bearer_auth(&self.config.openai_api_key)
            .multipart(form)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .context("transcription request failed")?;
        if !resp.status().is_success() {
            bail!("transcription API returned {}", error_body(resp).await);
        }
        let body: Value = resp.json().await?;
        Ok(body["text"].as_str().unwrap_or_default().trim().to_string())
    }

    pub async fn extract_recipe(&self, input: &ExtractInput<'_>) -> Result<Completion> {
        let mut content = vec![json!({
            "type": "text",
            "text": user_prompt(input, self.config.extra_prompt.as_deref()),
        })];
        content.extend(
            input
                .images
                .iter()
                .map(|url| json!({ "type": "image_url", "image_url": { "url": url } })),
        );
        self.structured(
            &self.config.text_model,
            SYSTEM_PROMPT,
            json!(content),
            "recipe",
            recipe_schema(),
        )
        .await
    }

    /// Runs a chat completion whose reply must match `schema` (strict JSON schema mode).
    pub async fn structured(
        &self,
        model: &str,
        system: &str,
        user: Value,
        name: &str,
        schema: Value,
    ) -> Result<Completion> {
        let body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": name, "strict": true, "schema": schema }
            }
        });
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.config.openai_url))
            .bearer_auth(&self.config.openai_api_key)
            .json(&body)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .with_context(|| format!("{name} request failed"))?;
        if !resp.status().is_success() {
            bail!("chat API returned {}", error_body(resp).await);
        }
        let chat: ChatResponse = resp.json().await.context("unexpected chat API response")?;
        let choice = chat
            .choices
            .into_iter()
            .next()
            .context("model returned no choices")?;
        if let Some(refusal) = choice.message.refusal {
            bail!("model refused: {refusal}");
        }
        if choice.finish_reason.as_deref() == Some("length") {
            bail!("model output was truncated (hit the token limit)");
        }
        let text = choice
            .message
            .content
            .context("model returned empty content")?;
        let value = serde_json::from_str(&text).context("model returned invalid JSON")?;
        Ok(Completion {
            value,
            prompt_tokens: chat.usage.as_ref().and_then(|u| u.prompt_tokens),
            completion_tokens: chat.usage.as_ref().and_then(|u| u.completion_tokens),
        })
    }
}

/// Strict mode needs every object to list all its properties as required and
/// forbid extras; this checks a schema recursively (used by tests).
#[cfg(test)]
pub fn assert_strict(schema: &Value, path: &str) {
    if let Some(props) = schema["properties"].as_object() {
        assert_eq!(
            schema["additionalProperties"], false,
            "{path} allows extra properties"
        );
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{path} has no required list"))
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for (key, prop) in props {
            assert!(
                required.contains(&key.as_str()),
                "{path}.{key} missing from required"
            );
            assert_strict(prop, &format!("{path}.{key}"));
        }
    }
    assert_strict_children(schema, path);
}

#[cfg(test)]
fn assert_strict_children(schema: &Value, path: &str) {
    if schema.get("items").is_some() {
        assert_strict(&schema["items"], &format!("{path}[]"));
    }
    for variant in schema["anyOf"].as_array().into_iter().flatten() {
        assert_strict(variant, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_every_property() {
        assert_strict(&recipe_schema(), "recipe");
    }

    fn extracted(ingredients: Value, steps: Value, names: Value) -> Value {
        json!({
            "is_recipe": true, "not_recipe_reason": null, "recipe_names": names,
            "recipeIngredient": ingredients, "recipeInstructions": steps,
        })
    }

    #[test]
    fn check_recipe_accepts_one_complete_recipe() {
        let ok = extracted(json!(["1 egg"]), json!([{"text": "Fry."}]), json!(["Egg"]));
        assert!(check_recipe(&ok, "post").is_ok());
        // Recipes extracted before recipe_names existed still pass.
        let mut old = ok.clone();
        old.as_object_mut().unwrap().remove("recipe_names");
        assert!(check_recipe(&old, "post").is_ok());
    }

    #[test]
    fn check_recipe_rejects_incomplete_or_several() {
        let err = |r: Value| check_recipe(&r, "post").unwrap_err().to_string();
        let steps = json!([{"text": "Fry."}]);
        assert!(err(extracted(json!([]), steps.clone(), json!(["Egg"]))).contains("no ingredients"));
        assert!(err(extracted(
            json!(["1 egg"]),
            json!([{"text": " "}]),
            json!(["Egg"])
        ))
        .contains("no instructions"));
        assert!(err(extracted(json!([" "]), json!([]), json!([])))
            .contains("no ingredients or instructions"));
        assert!(
            err(extracted(json!(["1 egg"]), steps, json!(["Egg", "Toast"])))
                .contains("2 separate recipes (Egg, Toast)")
        );
        let none = json!({"is_recipe": false, "not_recipe_reason": "just a vlog"});
        assert!(err(none).contains("just a vlog"));
    }

    #[test]
    fn prompt_includes_context_and_instructions() {
        let tags = vec!["dinner".to_string()];
        let prompt = user_prompt(
            &ExtractInput {
                url: "https://x/1",
                title: Some("Soup"),
                uploader: None,
                description: None,
                transcript: Some("add salt"),
                tags: &tags,
                note: Some("metric units"),
                images: &[],
            },
            Some("always english"),
        );
        assert!(prompt.contains("(no caption)"));
        assert!(prompt.contains("add salt"));
        assert!(prompt.contains("<keywords>dinner</keywords>"));
        assert!(prompt.contains("always english"));
        assert!(prompt.contains("metric units"));
    }
}
