//! Cleanup of imported recipes: every ingredient becomes a structured line linked
//! to a Mealie food (and unit), steps are tidied and linked to their ingredients,
//! and the name, description and times are made consistent.
//!
//! The model plans the cleanup; this module turns that plan into Mealie's recipe
//! shape. Food and unit lookups against Mealie happen in the worker.

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

pub const SYSTEM_PROMPT: &str = "You clean up recipes imported into Mealie so they are structured, readable and consistent.\n\
You receive the recipe as Mealie stored it and the list of measurement units that exist in Mealie.\n\
\n\
Ingredients: rebuild every ingredient line as a structured ingredient.\n\
- food is the plain base ingredient, singular and lowercase (\"unsalted butter\", \"garlic powder\", \"slider bun\", \"cheddar cheese\"). Prep words and sizes go in note, never in food: \"large yellow onions, diced\" -> food \"yellow onion\", note \"large, diced\". food_plural is its plural (\"yellow onions\").\n\
- Never combine two foods in one ingredient. Split combined lines: \"Salt and pepper, to taste\" -> two ingredients (salt, black pepper), no quantity, note \"to taste\".\n\
- unit is the full name of a unit from the supplied list (e.g. \"tablespoon\", never \"tbsp\"), or null. Countable items with no measure (\"12 slider buns\", \"2 eggs\") have a quantity and no unit.\n\
- Only when a genuine, reusable unit is missing from the list (e.g. slice, stick), use it and add it to new_units with its plural and abbreviation. Never add spelling variants or abbreviations of existing units.\n\
- quantity is a decimal number (1/3 -> 0.333, 1 1/2 -> 1.5) or null. For ranges use the lower number and keep the range in note (\"8-12 slices\"). For parenthetical equivalents like \"4 tablespoons (1/4 cup)\" keep one unit and drop the duplicate.\n\
- A different food the recipe allows instead goes in substitutions, never in note or food: \"1 cup chicken broth (or vegetable broth)\" -> food \"chicken broth\" with substitution food \"vegetable broth\"; \"butter or margarine\" -> food \"butter\" with substitution food \"margarine\". Put a caveat that only applies to the substitute in its note (\"use half as much\"), otherwise null. An alternative that is not a single food (\"water and a bouillon cube\") is a substitution with food null and the text as note. Lines marked (substitutes: ...) already have substitutions in Mealie: keep them. substitutions is [] when there are none.\n\
- Keep other optional info in note: \"optional\", \"or 1 cup shredded\" (another form of the same food), \"plus extra for serving\". note is \"\" when there is nothing to add.\n\
- If the recipe has components (sauce, dough, topping), set title on the first ingredient of each section; otherwise title is null.\n\
- Order ingredients in the order they are used. source_line is the number of the original line the ingredient came from (null if none).\n\
\n\
Instructions:\n\
- Imperative and clear, one logical action per step: split run-on steps, merge trivial ones. Someone should be able to follow them at the stove without re-reading.\n\
- Temperatures as \"350°F (175°C)\"; include times and doneness cues (\"until golden, 10-12 minutes\").\n\
- Remove creator chatter, sponsor mentions, \"link in bio\", \"part 5\", emojis.\n\
- Set title on steps that start a component section, matching the ingredient section titles (e.g. \"Sauce\", \"Assemble\"); otherwise null.\n\
- ingredients lists the positions (0-based) in your ingredients array of the ingredients used in that step.\n\
- Do not invent steps or change the recipe itself; only clarify. If something is too vague to follow, say so in notes rather than guessing.\n\
\n\
Metadata:\n\
- name is a descriptive Title Case dish name without creator handles or \"TikTok Recipe - Reply to ...\" (e.g. \"Air Fryer Breakfast Tacos\"). Keep useful qualifiers like \"(Vegan)\".\n\
- description is 1-2 sentences, neutral third person: what the dish is and what makes it notable. No hashtags, @mentions, emojis, \"recipe on my blog\" or filler.\n\
- recipe_yield (e.g. \"12 sliders\", \"4 servings\"), servings (a number) and prep_time, cook_time, total_time (human readable, e.g. \"15 minutes\", \"1 hour 10 minutes\") only when the source states them or they are clear from the recipe; otherwise null. Do not guess wildly.\n\
\n\
Categories: pick 1-3 categories for the dish from the supplied category list (e.g. meal type, course, cuisine, whatever the list covers), copying names exactly. Keep the recipe's current categories in mind and don't repeat them. Never invent categories; use [] when none fit or no list is supplied.\n\
\n\
Set cannot_clean to a short reason only if the recipe is too incomplete to clean (no ingredients or no instructions at all), or if it mixes several separate dishes that belong in separate recipes (components of one dish, like a sauce, dough or topping, are fine); otherwise null.\n\
Write in the language of the recipe.";

pub const MATCH_PROMPT: &str = "You link recipe ingredients to foods in a Mealie database.\n\
For each ingredient food you get candidate database foods. Pick the id of the candidate that is the same plain ingredient.\n\
- Prefer the cleanest, most generic entry. The database contains messy entries created by a parser (e.g. \"large yellow onions\", \"Salt and freshly ground black pepper\"); never pick entries that include quantities, sizes, preparation or two foods at once.\n\
- Singular/plural and capitalisation differences are fine. A more specific or different ingredient is not (\"brown sugar\" is not \"sugar\", \"garlic powder\" is not \"garlic\").\n\
- Use null when no candidate fits; a new food will be created.";

fn nullable(kind: &str) -> Value {
    json!({ "type": [kind, "null"] })
}

pub fn plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "cannot_clean", "name", "description", "recipe_yield", "servings", "prep_time",
            "cook_time", "total_time", "ingredients", "new_units", "instructions", "categories",
            "notes"
        ],
        "properties": {
            "cannot_clean": nullable("string"),
            "name": { "type": "string" },
            "description": { "type": "string" },
            "recipe_yield": nullable("string"),
            "servings": nullable("number"),
            "prep_time": nullable("string"),
            "cook_time": nullable("string"),
            "total_time": nullable("string"),
            "ingredients": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "title", "quantity", "unit", "food", "food_plural", "note", "substitutions",
                        "source_line"
                    ],
                    "properties": {
                        "title": nullable("string"),
                        "quantity": nullable("number"),
                        "unit": nullable("string"),
                        "food": { "type": "string" },
                        "food_plural": { "type": "string" },
                        "note": { "type": "string" },
                        "substitutions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["food", "food_plural", "note"],
                                "properties": {
                                    "food": nullable("string"),
                                    "food_plural": nullable("string"),
                                    "note": nullable("string")
                                }
                            }
                        },
                        "source_line": nullable("integer")
                    }
                }
            },
            "new_units": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "plural_name", "abbreviation"],
                    "properties": {
                        "name": { "type": "string" },
                        "plural_name": { "type": "string" },
                        "abbreviation": { "type": "string" }
                    }
                }
            },
            "instructions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "text", "ingredients"],
                    "properties": {
                        "title": nullable("string"),
                        "text": { "type": "string" },
                        "ingredients": { "type": "array", "items": { "type": "integer" } }
                    }
                }
            },
            "categories": { "type": "array", "items": { "type": "string" } },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

pub fn match_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["matches"],
        "properties": {
            "matches": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["food", "id"],
                    "properties": {
                        "food": { "type": "string" },
                        "id": nullable("string")
                    }
                }
            }
        }
    })
}

#[derive(Debug, Deserialize)]
pub struct Plan {
    pub cannot_clean: Option<String>,
    pub name: String,
    pub description: String,
    pub recipe_yield: Option<String>,
    pub servings: Option<f64>,
    pub prep_time: Option<String>,
    pub cook_time: Option<String>,
    pub total_time: Option<String>,
    pub ingredients: Vec<PlanIngredient>,
    pub new_units: Vec<NewUnit>,
    pub instructions: Vec<PlanStep>,
    #[serde(default)]
    pub categories: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PlanIngredient {
    pub title: Option<String>,
    pub quantity: Option<f64>,
    pub unit: Option<String>,
    pub food: String,
    pub food_plural: String,
    pub note: String,
    #[serde(default)]
    pub substitutions: Vec<PlanSubstitution>,
    pub source_line: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PlanSubstitution {
    pub food: Option<String>,
    pub food_plural: Option<String>,
    pub note: Option<String>,
}

impl Plan {
    /// Every food the plan uses, ingredients and substitutes, as (name, plural).
    pub fn foods(&self) -> Vec<(&str, &str)> {
        let mut foods = Vec::new();
        for ing in &self.ingredients {
            foods.push((ing.food.as_str(), ing.food_plural.as_str()));
            for sub in &ing.substitutions {
                if let Some(food) = sub.food.as_deref().filter(|f| !f.trim().is_empty()) {
                    foods.push((food, sub.food_plural.as_deref().unwrap_or_default()));
                }
            }
        }
        foods
    }
}

#[derive(Debug, Deserialize)]
pub struct NewUnit {
    pub name: String,
    pub plural_name: String,
    pub abbreviation: String,
}

#[derive(Debug, Deserialize)]
pub struct PlanStep {
    pub title: Option<String>,
    pub text: String,
    pub ingredients: Vec<i64>,
}

#[derive(Debug, Deserialize)]
pub struct Matches {
    pub matches: Vec<Match>,
}

#[derive(Debug, Deserialize)]
pub struct Match {
    pub food: String,
    pub id: Option<String>,
}

/// Lowercased, trimmed lookup key for food and unit names.
pub fn key(name: &str) -> String {
    name.trim().trim_end_matches('.').to_lowercase()
}

fn text(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn eq(v: &Value, key: &str) -> bool {
    text(v).is_some_and(|s| s.eq_ignore_ascii_case(key))
}

/// An ingredient line as Mealie currently stores it.
#[derive(Debug)]
pub struct Line {
    pub title: Option<String>,
    pub text: String,
    pub reference_id: Option<String>,
    /// Substitutions Mealie already stores on the line (3.28+).
    pub substitutions: Vec<Value>,
}

pub fn original_lines(recipe: &Value) -> Vec<Line> {
    recipe["recipeIngredient"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let text = text(&i["display"])
                .or_else(|| text(&i["note"]))
                .or_else(|| text(&i["originalText"]))?;
            Some(Line {
                title: text_owned(&i["title"]),
                text: text.to_string(),
                reference_id: text_owned(&i["referenceId"]),
                substitutions: i["substitutions"].as_array().cloned().unwrap_or_default(),
            })
        })
        .collect()
}

/// What a recipe lacks to be usable: "ingredients" and/or "instructions".
pub fn missing_parts(recipe: &Value) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if original_lines(recipe).is_empty() {
        missing.push("ingredients");
    }
    let has_steps = recipe["recipeInstructions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| text(&s["text"]).is_some());
    if !has_steps {
        missing.push("instructions");
    }
    missing
}

fn text_owned(v: &Value) -> Option<String> {
    text(v).map(str::to_string)
}

pub fn prompt(
    recipe: &Value,
    lines: &[Line],
    units: &[Value],
    categories: &[Value],
    extra: Option<&str>,
) -> String {
    let mut p = String::from("<recipe>\n");
    for (tag, field) in [
        ("name", "name"),
        ("description", "description"),
        ("yield", "recipeYield"),
        ("servings", "recipeServings"),
        ("prep_time", "prepTime"),
        ("cook_time", "performTime"),
        ("total_time", "totalTime"),
    ] {
        let value = match &recipe[field] {
            Value::Number(n) if n.as_f64().unwrap_or(0.0) > 0.0 => n.to_string(),
            v => text(v).unwrap_or_default().to_string(),
        };
        if !value.is_empty() {
            p += &format!("<{tag}>{value}</{tag}>\n");
        }
    }
    p += "<ingredients>\n";
    for (i, line) in lines.iter().enumerate() {
        if let Some(title) = &line.title {
            p += &format!("[section: {title}]\n");
        }
        let subs: Vec<&str> = line
            .substitutions
            .iter()
            .filter_map(|s| text(&s["substituteFood"]["name"]).or_else(|| text(&s["note"])))
            .collect();
        if subs.is_empty() {
            p += &format!("{i}: {}\n", line.text);
        } else {
            p += &format!("{i}: {} (substitutes: {})\n", line.text, subs.join("; "));
        }
    }
    p += "</ingredients>\n<instructions>\n";
    let steps = recipe["recipeInstructions"]
        .as_array()
        .into_iter()
        .flatten();
    for (i, step) in steps.filter(|s| text(&s["text"]).is_some()).enumerate() {
        let title = text(&step["title"])
            .map(|t| format!("[{t}] "))
            .unwrap_or_default();
        p += &format!(
            "{}. {title}{}\n",
            i + 1,
            text(&step["text"]).unwrap_or_default()
        );
    }
    p += "</instructions>\n</recipe>\n<units>\n";
    for unit in units {
        let name = text(&unit["name"]).unwrap_or_default();
        match text(&unit["pluralName"]).filter(|pl| !pl.eq_ignore_ascii_case(name)) {
            Some(plural) => p += &format!("{name} ({plural})\n"),
            None => p += &format!("{name}\n"),
        }
    }
    p += "</units>\n<categories>\n";
    for category in categories.iter().filter_map(|c| text(&c["name"])) {
        p += &format!("{category}\n");
    }
    p += "</categories>\n";
    let current: Vec<&str> = recipe["recipeCategory"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| text(&c["name"]))
        .collect();
    if !current.is_empty() {
        p += &format!(
            "<current_categories>{}</current_categories>\n",
            current.join(", ")
        );
    }
    if let Some(extra) = extra {
        p += &format!("<user_instructions>{extra}</user_instructions>\n");
    }
    p
}

/// Drops duplicate units named after another unit's abbreviation (e.g. a unit
/// called "tbsp" next to "tablespoon"), so only the canonical one gets used.
pub fn usable_units(units: Vec<Value>) -> Vec<Value> {
    let duplicate = |u: &Value| {
        let name = text(&u["name"]).unwrap_or_default();
        units.iter().any(|other| {
            !eq(&other["name"], name)
                && (eq(&other["abbreviation"], name) || eq(&other["pluralAbbreviation"], name))
        })
    };
    units
        .iter()
        .filter(|u| text(&u["name"]).is_some() && !duplicate(u))
        .cloned()
        .collect()
}

fn has_alias(v: &Value, key: &str) -> bool {
    v["aliases"].as_array().is_some_and(|a| {
        a.iter()
            .any(|alias| alias["name"].as_str().is_some_and(|n| self::key(n) == key))
    })
}

/// Finds a unit by name, plural, alias or abbreviation (names win over abbreviations).
pub fn find_unit<'a>(units: &'a [Value], name: &str) -> Option<&'a Value> {
    let k = key(name);
    units
        .iter()
        .find(|u| eq(&u["name"], &k) || eq(&u["pluralName"], &k) || has_alias(u, &k))
        .or_else(|| {
            units
                .iter()
                .find(|u| eq(&u["abbreviation"], &k) || eq(&u["pluralAbbreviation"], &k))
        })
}

/// A database food whose name or alias is exactly this ingredient (ignoring case and plural).
pub fn exact_food<'a>(candidates: &'a [Value], name: &str, plural: &str) -> Option<&'a Value> {
    let (name, plural) = (key(name), key(plural));
    candidates.iter().find(|f| {
        eq(&f["name"], &name)
            || eq(&f["pluralName"], &name)
            || has_alias(f, &name)
            || (!plural.is_empty() && (eq(&f["name"], &plural) || has_alias(f, &plural)))
    })
}

fn clean_note(note: Option<&str>) -> Option<String> {
    note.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// The recipe-level substitutions for one ingredient: the plan's, then any the
/// original line already had. Drops substitutes equal to the food itself,
/// duplicates, and ones the food already lists as a food-level substitution.
fn substitutions(
    ing: &PlanIngredient,
    food: &Value,
    carried: &[Value],
    foods: &HashMap<String, Value>,
) -> Vec<Value> {
    let planned = ing.substitutions.iter().map(|s| {
        let id = s
            .food
            .as_deref()
            .and_then(|f| foods.get(&key(f)))
            .and_then(|f| text(&f["id"]))
            .map(str::to_string);
        (id, clean_note(s.note.as_deref()))
    });
    let existing = carried.iter().map(|s| {
        (
            text_owned(&s["substituteFoodId"]),
            clean_note(s["note"].as_str()),
        )
    });
    let food_level: Vec<(Option<String>, Option<String>)> = food["substitutions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            (
                text_owned(&s["substituteFoodId"]),
                clean_note(s["note"].as_str()),
            )
        })
        .collect();
    let food_id = text(&food["id"]);

    let mut out: Vec<(Option<String>, Option<String>)> = Vec::new();
    for (id, note) in planned.chain(existing) {
        let duplicate = match &id {
            None if note.is_none() => true,
            None => out.iter().any(|(i, n)| i.is_none() && *n == note),
            Some(id) => {
                Some(id.as_str()) == food_id
                    || out.iter().any(|(i, _)| i.as_ref() == Some(id))
                    || food_level
                        .iter()
                        .any(|(i, n)| i.as_ref() == Some(id) && (note.is_none() || *n == note))
            }
        };
        if !duplicate {
            out.push((id, note));
        }
    }
    out.into_iter()
        .map(|(id, note)| json!({ "substituteFoodId": id, "note": note }))
        .collect()
}

/// Search terms for finding a food: the full name, then its head noun.
pub fn search_terms(name: &str) -> Vec<String> {
    let name = key(name);
    let mut terms = vec![name.clone()];
    if let Some(last) = name.split_whitespace().last().filter(|l| *l != name) {
        terms.push(last.to_string());
    }
    terms
}

pub fn match_prompt(pending: &[(String, Vec<Value>)]) -> String {
    let mut p = String::new();
    for (food, candidates) in pending {
        p += &format!("<ingredient food=\"{food}\">\n");
        for c in candidates {
            p += &format!(
                "{}: {}\n",
                text(&c["id"]).unwrap_or_default(),
                text(&c["name"]).unwrap_or_default()
            );
        }
        p += "</ingredient>\n";
    }
    p
}

pub struct Built {
    pub ingredients: Vec<Value>,
    pub instructions: Vec<Value>,
    pub warnings: Vec<String>,
}

/// Turns the plan into Mealie ingredients and steps. `foods` must hold every
/// planned food by key; `units` holds the resolved units by key.
pub fn build(
    plan: &Plan,
    lines: &[Line],
    foods: &HashMap<String, Value>,
    units: &HashMap<String, Value>,
) -> Result<Built> {
    let mut warnings = Vec::new();
    let mut used_refs: Vec<String> = Vec::new();
    let mut ingredients = Vec::new();
    for ing in &plan.ingredients {
        let Some(food) = foods.get(&key(&ing.food)) else {
            bail!("no food resolved for \"{}\"", ing.food);
        };
        let mut note = ing.note.trim().to_string();
        let unit = match ing.unit.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(name) => match units.get(&key(name)) {
                Some(unit) => unit.clone(),
                None => {
                    warnings.push(format!("unknown unit \"{name}\" moved into the note"));
                    note = if note.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}, {note}")
                    };
                    Value::Null
                }
            },
            None => Value::Null,
        };
        let line = ing
            .source_line
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| lines.get(i));
        // Keep the original reference when a line maps to one ingredient, so
        // links Mealie already has stay valid; split lines get fresh ids.
        let original_ref = line
            .and_then(|l| l.reference_id.clone())
            .filter(|r| !used_refs.contains(r));
        // Substitutions already on the line follow its first ingredient.
        let carried = match (&original_ref, line) {
            (Some(_), Some(l)) => l.substitutions.as_slice(),
            _ => &[],
        };
        let substitutions = substitutions(ing, food, carried, foods);
        let reference_id = original_ref.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        used_refs.push(reference_id.clone());
        ingredients.push(json!({
            "referenceId": reference_id,
            "title": ing.title.as_deref().map(str::trim).unwrap_or_default(),
            "quantity": ing.quantity.unwrap_or(0.0),
            "unit": unit,
            "food": food,
            "note": note,
            "substitutions": substitutions,
            "originalText": line.map(|l| l.text.as_str()),
            "isFood": true,
            "disableAmount": false,
        }));
    }

    let instructions = plan
        .instructions
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|step| {
            let refs: Vec<Value> = step
                .ingredients
                .iter()
                .filter_map(|i| usize::try_from(*i).ok())
                .filter_map(|i| ingredients.get(i))
                .map(|ing| json!({ "referenceId": ing["referenceId"] }))
                .collect();
            json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "title": step.title.as_deref().map(str::trim).unwrap_or_default(),
                "summary": "",
                "text": step.text.trim(),
                "ingredientReferences": refs,
            })
        })
        .collect();

    Ok(Built {
        ingredients,
        instructions,
        warnings,
    })
}

/// Writes the plan's metadata and the built lists onto a full Mealie recipe.
/// Fields the plan leaves empty keep their current value.
pub fn apply(recipe: &mut Value, plan: &Plan, built: Built) {
    let set = |recipe: &mut Value, field: &str, value: Option<&str>| {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            recipe[field] = json!(v);
        }
    };
    set(recipe, "name", Some(&plan.name));
    set(recipe, "description", Some(&plan.description));
    set(recipe, "recipeYield", plan.recipe_yield.as_deref());
    set(recipe, "prepTime", plan.prep_time.as_deref());
    // Mealie shows performTime as "Cook Time".
    set(recipe, "performTime", plan.cook_time.as_deref());
    set(recipe, "totalTime", plan.total_time.as_deref());
    if let Some(servings) = plan.servings.filter(|s| *s > 0.0) {
        recipe["recipeServings"] = json!(servings);
    }
    recipe["recipeIngredient"] = Value::Array(built.ingredients);
    recipe["recipeInstructions"] = Value::Array(built.instructions);
    if recipe["settings"].is_object() {
        // Older Mealie versions hide quantities unless this is off.
        recipe["settings"]["disableAmount"] = json!(false);
    }
}

/// Adds the plan's categories (matched by name against Mealie's existing ones)
/// to the recipe, keeping the ones it already has. Returns the names added and
/// the names that matched no existing category.
pub fn add_categories(
    recipe: &mut Value,
    plan: &Plan,
    available: &[Value],
) -> (Vec<String>, Vec<String>) {
    let mut current: Vec<Value> = recipe["recipeCategory"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let (mut added, mut unknown) = (Vec::new(), Vec::new());
    for name in plan
        .categories
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
    {
        let Some(category) = available.iter().find(|c| eq(&c["name"], name)) else {
            unknown.push(name.to_string());
            continue;
        };
        if current.iter().any(|c| c["id"] == category["id"]) {
            continue;
        }
        current.push(json!({
            "id": category["id"],
            "name": category["name"],
            "slug": category["slug"],
        }));
        added.push(text(&category["name"]).unwrap_or(name).to_string());
    }
    recipe["recipeCategory"] = Value::Array(current);
    (added, unknown)
}

/// Problems in a saved recipe that mean the cleanup didn't take.
pub fn verify(recipe: &Value, units: &[Value]) -> Vec<String> {
    let mut problems = Vec::new();
    for ing in recipe["recipeIngredient"].as_array().into_iter().flatten() {
        let label = text(&ing["display"])
            .or_else(|| text(&ing["note"]))
            .unwrap_or("?");
        if text(&ing["food"]["id"]).is_none() {
            problems.push(format!("\"{label}\" has no food"));
        }
        if let Some(unit) = text(&ing["unit"]["id"]) {
            if !units.iter().any(|u| eq(&u["id"], unit)) {
                problems.push(format!("\"{label}\" uses a duplicate unit"));
            }
        }
    }
    if text(&recipe["description"]).is_some_and(|d| d.contains('#')) {
        problems.push("description still contains hashtags".into());
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::assert_strict;

    fn units() -> Vec<Value> {
        vec![
            json!({"id": "u1", "name": "tablespoon", "pluralName": "tablespoons", "abbreviation": "tbsp"}),
            json!({"id": "u2", "name": "tbsp", "pluralName": null, "abbreviation": ""}),
            json!({"id": "u3", "name": "cup", "pluralName": "cups", "abbreviation": "c"}),
        ]
    }

    #[test]
    fn missing_parts_finds_empty_ingredients_and_steps() {
        let full = json!({
            "recipeIngredient": [{"display": "1 egg"}],
            "recipeInstructions": [{"text": "Fry."}],
        });
        assert!(missing_parts(&full).is_empty());
        let empty = json!({
            "recipeIngredient": [{"display": "", "note": null}],
            "recipeInstructions": [{"text": "  "}],
        });
        assert_eq!(missing_parts(&empty), ["ingredients", "instructions"]);
        assert_eq!(missing_parts(&json!({})), ["ingredients", "instructions"]);
    }

    #[test]
    fn schemas_are_strict() {
        assert_strict(&plan_schema(), "plan");
        assert_strict(&match_schema(), "match");
    }

    #[test]
    fn drops_abbreviation_duplicate_units() {
        let usable = usable_units(units());
        let names: Vec<&str> = usable.iter().map(|u| u["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["tablespoon", "cup"]);
        assert_eq!(find_unit(&usable, "Tbsp").unwrap()["id"], "u1");
        assert_eq!(find_unit(&usable, "cups").unwrap()["id"], "u3");
        assert!(find_unit(&usable, "slice").is_none());
    }

    #[test]
    fn finds_exact_foods_and_search_terms() {
        let foods = vec![
            json!({"id": "f1", "name": "large yellow onions"}),
            json!({"id": "f2", "name": "Yellow Onion", "pluralName": "yellow onions"}),
        ];
        assert_eq!(
            exact_food(&foods, "yellow onion", "yellow onions").unwrap()["id"],
            "f2"
        );
        assert!(exact_food(&foods, "onion", "onions").is_none());
        assert_eq!(search_terms("Yellow Onion"), ["yellow onion", "onion"]);
        assert_eq!(search_terms("salt"), ["salt"]);
    }

    #[test]
    fn builds_ingredients_with_links_and_references() {
        let recipe = json!({
            "name": "tiktok recipe #foodtok",
            "settings": {"disableAmount": true},
            "recipeIngredient": [
                {"display": "2 tbsp butter", "referenceId": "r0"},
                {"note": "Salt and pepper, to taste", "referenceId": "r1"},
            ],
            "recipeInstructions": [{"text": "Melt butter. Season."}],
        });
        let lines = original_lines(&recipe);
        assert_eq!(lines[1].text, "Salt and pepper, to taste");
        let categories = vec![json!({"id": "c1", "name": "Dinner", "slug": "dinner"})];
        let prompt = prompt(&recipe, &lines, &usable_units(units()), &categories, None);
        assert!(prompt.contains("<categories>\nDinner\n</categories>"));
        assert!(prompt.contains("1: Salt and pepper, to taste"));
        assert!(prompt.contains("tablespoon (tablespoons)"));
        assert!(!prompt.contains("\ntbsp\n"));

        let plan: Plan = serde_json::from_value(json!({
            "cannot_clean": null,
            "name": "Brown Butter",
            "description": "Nutty butter.",
            "recipe_yield": null, "servings": 2, "prep_time": null, "cook_time": "5 minutes", "total_time": null,
            "ingredients": [
                {"title": null, "quantity": 2, "unit": "tablespoon", "food": "Butter", "food_plural": "butters", "note": "", "source_line": 0},
                {"title": null, "quantity": null, "unit": null, "food": "salt", "food_plural": "salt", "note": "to taste", "source_line": 1},
                {"title": null, "quantity": null, "unit": "pinch-ish", "food": "black pepper", "food_plural": "black pepper", "note": "to taste", "source_line": 1},
            ],
            "new_units": [],
            "instructions": [
                {"title": null, "text": "Melt the butter.", "ingredients": [0]},
                {"title": null, "text": "Season with salt and pepper.", "ingredients": [1, 2, 9]},
            ],
            "notes": [],
        }))
        .unwrap();
        let foods: HashMap<String, Value> = [
            ("butter", json!({"id": "f1", "name": "butter"})),
            ("salt", json!({"id": "f2", "name": "salt"})),
            ("black pepper", json!({"id": "f3", "name": "black pepper"})),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let units: HashMap<String, Value> = [("tablespoon".to_string(), units()[0].clone())].into();

        let built = build(&plan, &lines, &foods, &units).unwrap();
        assert_eq!(built.warnings.len(), 1);
        let ings = &built.ingredients;
        assert_eq!(ings[0]["referenceId"], "r0");
        assert_eq!(ings[0]["unit"]["id"], "u1");
        assert_eq!(ings[1]["referenceId"], "r1");
        assert_ne!(
            ings[2]["referenceId"], "r1",
            "split lines get new references"
        );
        assert_eq!(ings[2]["unit"], Value::Null);
        assert_eq!(ings[2]["note"], "pinch-ish, to taste");
        let steps = &built.instructions;
        assert_eq!(
            steps[0]["ingredientReferences"],
            json!([{"referenceId": "r0"}])
        );
        assert_eq!(
            steps[1]["ingredientReferences"].as_array().unwrap().len(),
            2
        );

        let mut recipe = recipe;
        apply(&mut recipe, &plan, built);
        assert_eq!(recipe["name"], "Brown Butter");
        assert_eq!(recipe["performTime"], "5 minutes");
        assert_eq!(recipe["recipeServings"], 2.0);
        assert_eq!(recipe["settings"]["disableAmount"], false);
        assert!(recipe.get("recipeYield").is_none());
        assert!(verify(&recipe, &usable_units(super::tests::units())).is_empty());

        recipe["recipeIngredient"][0]["food"] = Value::Null;
        recipe["recipeIngredient"][1]["unit"] = json!({"id": "u2", "name": "tbsp"});
        assert_eq!(
            verify(&recipe, &usable_units(super::tests::units())).len(),
            2
        );
    }

    #[test]
    fn matches_aliases() {
        let foods =
            vec![json!({"id": "f1", "name": "scallion", "aliases": [{"name": "green onion"}]})];
        assert_eq!(
            exact_food(&foods, "green onion", "green onions").unwrap()["id"],
            "f1"
        );
        let units = vec![json!({"id": "u1", "name": "teaspoon", "aliases": [{"name": "tsp."}]})];
        assert_eq!(find_unit(&units, "tsp.").unwrap()["id"], "u1");
    }

    #[test]
    fn builds_substitutions() {
        let recipe = json!({
            "recipeIngredient": [
                {"display": "1 cup chicken broth (or veggie broth)", "referenceId": "r0"},
                {"display": "2 tbsp butter", "referenceId": "r1", "substitutions": [
                    {"substituteFoodId": "f-oil", "note": null, "substituteFood": {"id": "f-oil", "name": "olive oil"}},
                    {"substituteFoodId": null, "note": "ghee works too"},
                ]},
            ],
        });
        let lines = original_lines(&recipe);
        let prompt = prompt(&recipe, &lines, &[], &[], None);
        assert!(prompt.contains("1: 2 tbsp butter (substitutes: olive oil; ghee works too)"));

        let plan: Plan = serde_json::from_value(json!({
            "cannot_clean": null, "name": "x", "description": "", "recipe_yield": null, "servings": null,
            "prep_time": null, "cook_time": null, "total_time": null,
            "ingredients": [
                {"title": null, "quantity": 1, "unit": null, "food": "chicken broth", "food_plural": "chicken broth",
                 "note": "", "source_line": 0, "substitutions": [
                    {"food": "vegetable broth", "food_plural": "vegetable broth", "note": null},
                    {"food": "chicken broth", "food_plural": null, "note": null},
                    {"food": null, "food_plural": null, "note": "water and a bouillon cube"},
                 ]},
                {"title": null, "quantity": 2, "unit": null, "food": "butter", "food_plural": "butter",
                 "note": "", "source_line": 1, "substitutions": [
                    {"food": "margarine", "food_plural": "margarine", "note": null},
                    {"food": "olive oil", "food_plural": "olive oil", "note": null},
                 ]},
            ],
            "new_units": [], "instructions": [], "notes": [],
        }))
        .unwrap();
        let names: Vec<&str> = plan.foods().iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "chicken broth",
                "vegetable broth",
                "chicken broth",
                "butter",
                "margarine",
                "olive oil"
            ]
        );

        let foods: HashMap<String, Value> = [
            ("chicken broth", json!({"id": "f-cb", "name": "chicken broth"})),
            ("vegetable broth", json!({"id": "f-vb", "name": "vegetable broth"})),
            // Butter already lists margarine as a food-level substitution.
            ("butter", json!({"id": "f-b", "name": "butter", "substitutions": [{"substituteFoodId": "f-m", "note": null}]})),
            ("margarine", json!({"id": "f-m", "name": "margarine"})),
            ("olive oil", json!({"id": "f-oil", "name": "olive oil"})),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let built = build(&plan, &lines, &foods, &HashMap::new()).unwrap();
        assert_eq!(
            built.ingredients[0]["substitutions"],
            json!([
                {"substituteFoodId": "f-vb", "note": null},
                {"substituteFoodId": null, "note": "water and a bouillon cube"},
            ])
        );
        assert_eq!(
            built.ingredients[1]["substitutions"],
            json!([
                {"substituteFoodId": "f-oil", "note": null},
                {"substituteFoodId": null, "note": "ghee works too"},
            ])
        );
    }

    #[test]
    fn adds_only_existing_categories() {
        let available = vec![
            json!({"id": "c1", "name": "Dinner", "slug": "dinner", "recipeCount": 4}),
            json!({"id": "c2", "name": "Italian", "slug": "italian"}),
        ];
        let mut recipe =
            json!({"recipeCategory": [{"id": "c1", "name": "Dinner", "slug": "dinner"}]});
        let plan: Plan = serde_json::from_value(json!({
            "cannot_clean": null, "name": "x", "description": "", "recipe_yield": null, "servings": null,
            "prep_time": null, "cook_time": null, "total_time": null, "ingredients": [], "new_units": [],
            "instructions": [], "categories": ["dinner", "italian", "Pasta Night"], "notes": [],
        }))
        .unwrap();
        let (added, unknown) = add_categories(&mut recipe, &plan, &available);
        assert_eq!(added, ["Italian"]);
        assert_eq!(unknown, ["Pasta Night"]);
        assert_eq!(
            recipe["recipeCategory"],
            json!([
                {"id": "c1", "name": "Dinner", "slug": "dinner"},
                {"id": "c2", "name": "Italian", "slug": "italian"},
            ])
        );
    }

    #[test]
    fn missing_food_is_an_error() {
        let plan: Plan = serde_json::from_value(json!({
            "cannot_clean": null, "name": "x", "description": "", "recipe_yield": null, "servings": null,
            "prep_time": null, "cook_time": null, "total_time": null,
            "ingredients": [{"title": null, "quantity": 1, "unit": null, "food": "egg", "food_plural": "eggs", "note": "", "source_line": null}],
            "new_units": [], "instructions": [], "notes": [],
        }))
        .unwrap();
        assert!(build(&plan, &[], &HashMap::new(), &HashMap::new()).is_err());
    }
}
