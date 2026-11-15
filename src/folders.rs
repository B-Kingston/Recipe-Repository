//! Library folders: the library's Folders and Cuisines views file every
//! recipe in one folder of a fixed list, by EmbeddingGemma. Each recipe's
//! search vector (`recipe_embeddings`) is compared with folder prototypes
//! embedded once per process: a dish folder is a handful of example dishes
//! ("Desserts and sweets: cheesecake"), and the recipe goes to the folder
//! whose two closest examples score best; a cuisine is one "Thai food"
//! query, and a recipe with no clear winner goes to Other. The cook can
//! pin a recipe to a folder or cuisine on its page (`recipes.folder`,
//! `recipes.cuisine`; empty means sorted automatically). A recipe without
//! a vector yet, or every recipe when the embedding sidecar is missing,
//! waits in "Not sorted yet" unless pinned.
//!
//! Measured on 55 test recipes against the live sidecar: dish folders 47/55
//! (the misses are neighbours such as a noodle soup filed with noodles),
//! cuisines 45/48 with most dishes of no particular cuisine going to Other.

use crate::embeddings::{self, Embedder, Input};
use crate::recipes::{self, LibraryCard};
use crate::{AppError, AppState, AuthUser, Result, app_setting, set_app_setting};
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::OnceCell;
use tracing::{info, warn};

/// One folder of a fixed list.
pub(crate) struct Folder {
    pub(crate) slug: &'static str,
    pub(crate) name: &'static str,
    /// The folder as the prototypes phrase it.
    topic: &'static str,
    /// Example dishes of a dish folder; empty for a cuisine.
    examples: &'static [&'static str],
}

const fn dish(
    slug: &'static str,
    name: &'static str,
    topic: &'static str,
    examples: &'static [&'static str],
) -> Folder {
    Folder {
        slug,
        name,
        topic,
        examples,
    }
}

const fn cuisine(slug: &'static str, name: &'static str) -> Folder {
    Folder {
        slug,
        name,
        topic: name,
        examples: &[],
    }
}

/// The dish folders, in the order the library shows them. Keep the
/// examples clear of each other's dishes: they decide the filing.
pub(crate) const DISH_FOLDERS: &[Folder] = &[
    dish(
        "breakfast",
        "Breakfast",
        "Breakfast and brunch",
        &[
            "eggs benedict",
            "porridge",
            "granola",
            "scrambled eggs on toast",
            "waffles",
            "breakfast burrito",
            "bacon and egg muffin",
            "huevos rancheros",
        ],
    ),
    dish(
        "starters",
        "Snacks & starters",
        "Snacks and starters",
        &[
            "spring rolls",
            "bruschetta",
            "nachos",
            "samosas",
            "baba ganoush dip",
            "chicken wings",
            "arancini",
            "crostini",
        ],
    ),
    dish(
        "soups",
        "Soups & stews",
        "Soups and stews",
        &[
            "french onion soup",
            "chicken noodle soup",
            "lentil soup",
            "beef stew",
            "gumbo",
            "laksa",
            "chilli con carne",
            "irish stew",
        ],
    ),
    dish(
        "salads",
        "Salads",
        "Salads",
        &[
            "caprese salad",
            "coleslaw",
            "nicoise salad",
            "potato salad",
            "fattoush",
            "quinoa salad",
            "waldorf salad",
            "larb",
        ],
    ),
    dish(
        "pasta",
        "Pasta & noodles",
        "Pasta and noodles",
        &[
            "spaghetti carbonara",
            "penne arrabbiata",
            "mac and cheese",
            "chow mein",
            "udon stir fry",
            "lo mein",
            "gnocchi",
            "ravioli",
        ],
    ),
    dish(
        "rice",
        "Rice & grains",
        "Rice and grains",
        &[
            "biryani",
            "jambalaya",
            "nasi goreng",
            "pilaf",
            "sushi rice bowl",
            "jollof rice",
            "kedgeree",
            "congee",
        ],
    ),
    dish(
        "chicken",
        "Chicken & poultry",
        "Chicken and poultry",
        &[
            "chicken parmesan",
            "roast turkey",
            "chicken tikka",
            "fried chicken",
            "coq au vin",
            "chicken fajitas",
            "duck confit",
            "chicken schnitzel",
        ],
    ),
    dish(
        "meat",
        "Beef, pork & lamb",
        "Beef, pork and lamb",
        &[
            "steak",
            "pulled pork",
            "meatballs",
            "roast lamb",
            "pork chops",
            "beef wellington",
            "sausages and mash",
            "ribs",
        ],
    ),
    dish(
        "seafood",
        "Fish & seafood",
        "Fish and seafood",
        &[
            "grilled salmon",
            "fish tacos",
            "mussels",
            "fish pie",
            "ceviche",
            "seared scallops",
            "tuna steak",
            "prawn cocktail",
        ],
    ),
    dish(
        "vegetarian",
        "Vegetarian",
        "Vegetarian mains",
        &[
            "vegetable curry",
            "dal",
            "falafel",
            "stuffed peppers",
            "vegetable stir fry",
            "bean chilli",
            "tofu stir fry",
            "aubergine parmigiana",
        ],
    ),
    dish(
        "sides",
        "Sides",
        "Side dishes",
        &[
            "mashed potatoes",
            "roasted vegetables",
            "steamed greens",
            "chips",
            "creamed spinach",
            "cauliflower cheese",
            "glazed carrots",
            "corn on the cob",
        ],
    ),
    dish(
        "baking",
        "Breads & baking",
        "Breads and baking",
        &[
            "naan",
            "bagels",
            "cinnamon rolls",
            "pizza dough",
            "banana bread",
            "scones",
            "pita bread",
            "brioche",
        ],
    ),
    dish(
        "desserts",
        "Desserts & sweets",
        "Desserts and sweets",
        &[
            "chocolate cake",
            "chocolate chip cookies",
            "cheesecake",
            "apple pie",
            "ice cream",
            "creme brulee",
            "panna cotta",
            "fruit crumble",
        ],
    ),
    dish(
        "drinks",
        "Drinks",
        "Drinks",
        &[
            "smoothie",
            "margarita",
            "hot chocolate",
            "lemonade",
            "chai tea",
            "mojito",
            "milkshake",
            "bubble tea",
        ],
    ),
    dish(
        "sauces",
        "Sauces & condiments",
        "Sauces and condiments",
        &[
            "salsa",
            "tomato sauce",
            "vinaigrette",
            "gravy",
            "mayonnaise",
            "hot sauce",
            "tzatziki",
            "spice rub",
        ],
    ),
];

/// The cuisines, in the order the library shows them; Other comes last.
pub(crate) const CUISINES: &[Folder] = &[
    cuisine("italian", "Italian"),
    cuisine("french", "French"),
    cuisine("spanish", "Spanish"),
    cuisine("greek", "Greek"),
    cuisine("middle-eastern", "Middle Eastern"),
    cuisine("indian", "Indian"),
    cuisine("thai", "Thai"),
    cuisine("vietnamese", "Vietnamese"),
    cuisine("chinese", "Chinese"),
    cuisine("japanese", "Japanese"),
    cuisine("korean", "Korean"),
    cuisine("mexican", "Mexican"),
    cuisine("american", "American"),
    cuisine("british", "British"),
];

/// Recipes of no particular cuisine (or no clear one).
pub(crate) const OTHER_CUISINE: Folder = cuisine("other", "Other");
/// Recipes the view cannot file yet: no vector, or no embedding sidecar.
pub(crate) const UNSORTED: Folder = cuisine("unsorted", "Not sorted yet");

/// A cuisine needs at least this score: an iced latte topped out at 0.57.
pub(crate) const CUISINE_MIN_SCORE: f32 = 0.60;
/// And must beat the runner-up by this much; plain dishes (overnight oats,
/// garlic prawns, a sourdough loaf) won by 0.005-0.006.
pub(crate) const CUISINE_MARGIN: f32 = 0.010;
/// A dish folder scores the mean of a recipe's closest examples, this many.
const CLOSEST_EXAMPLES: usize = 2;
/// The top ticket of a folder stack lists this many dishes.
pub(crate) const STACK_TITLES: usize = 4;

/// The library's two folder views.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shelf {
    Dishes,
    Cuisines,
}

impl Shelf {
    /// The folders a recipe can be filed in, Other included for cuisines.
    pub(crate) fn folders(self) -> Vec<&'static Folder> {
        match self {
            Shelf::Dishes => DISH_FOLDERS.iter().collect(),
            Shelf::Cuisines => CUISINES.iter().chain([&OTHER_CUISINE]).collect(),
        }
    }

    /// The view's name, as the toggle and the open folder's eyebrow say it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Shelf::Dishes => "Folders",
            Shelf::Cuisines => "Cuisines",
        }
    }

    /// The `library_view` setting value.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Shelf::Dishes => VIEW_FOLDERS,
            Shelf::Cuisines => VIEW_CUISINES,
        }
    }

    /// The folder with this slug, Not sorted yet included.
    pub(crate) fn folder(self, slug: &str) -> Option<&'static Folder> {
        if slug == UNSORTED.slug {
            return Some(&UNSORTED);
        }
        self.pinnable(slug)
    }

    /// The folder with this slug that a recipe can be filed and pinned in:
    /// any of `folders`, never Not sorted yet.
    pub(crate) fn pinnable(self, slug: &str) -> Option<&'static Folder> {
        let (folders, other) = match self {
            Shelf::Dishes => (DISH_FOLDERS, None),
            Shelf::Cuisines => (CUISINES, Some(&OTHER_CUISINE)),
        };
        folders
            .iter()
            .chain(other)
            .find(|folder| folder.slug == slug)
    }
}

/// app_settings key of the library's view: "az" (absent too), "folders" or
/// "cuisines".
const VIEW_SETTING: &str = "library_view";
const VIEW_AZ: &str = "az";
const VIEW_FOLDERS: &str = "folders";
const VIEW_CUISINES: &str = "cuisines";
/// The Group view: every recipe the members of the user's group share.
pub(crate) const VIEW_GROUP: &str = "group";

/// The user's library view: None files A to Z.
pub(crate) async fn library_view(db: &sqlx::SqlitePool, user_id: &str) -> Result<Option<Shelf>> {
    Ok(
        match app_setting(db, user_id, VIEW_SETTING).await?.as_deref() {
            Some(VIEW_FOLDERS) => Some(Shelf::Dishes),
            Some(VIEW_CUISINES) => Some(Shelf::Cuisines),
            _ => None,
        },
    )
}

/// Whether the user's saved library view is the Group view (which applies
/// only while they are in a group).
pub(crate) async fn group_view_selected(db: &sqlx::SqlitePool, user_id: &str) -> Result<bool> {
    Ok(app_setting(db, user_id, VIEW_SETTING).await?.as_deref() == Some(VIEW_GROUP))
}

#[derive(Deserialize)]
pub(crate) struct ViewForm {
    view: String,
}

/// `POST /library/view`: the A–Z / Folders / Cuisines / Group toggle on the
/// sign.
pub(crate) async fn update_library_view(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Form(form): Form<ViewForm>,
) -> Result<Response> {
    let view = match form.view.as_str() {
        VIEW_AZ | VIEW_FOLDERS | VIEW_CUISINES | VIEW_GROUP => form.view.as_str(),
        _ => return Err(AppError::BadRequest("Unknown library view.".into())),
    };
    set_app_setting(&state.db, &user.id, VIEW_SETTING, view).await?;
    Ok(Redirect::to("/").into_response())
}

/// The folder an open-folder link (`/?folder=`) names: a dish folder or a
/// cuisine (Other included), or Not sorted yet in the current view.
pub(crate) fn resolve(slug: &str, view: Option<Shelf>) -> Option<(Shelf, &'static Folder)> {
    if slug == UNSORTED.slug {
        return Some((view.unwrap_or(Shelf::Dishes), &UNSORTED));
    }
    [Shelf::Dishes, Shelf::Cuisines]
        .into_iter()
        .find_map(|shelf| shelf.folder(slug).map(|folder| (shelf, folder)))
}

/// Words that end a folder search without changing it ("thai food").
const QUERY_TAILS: &[&str] = &["food", "cuisine", "recipe", "dish", "meal"];

/// The folder a search names outright: "thai", "Thai food", "dessert",
/// "soups and stews", "pork". Words are compared after `search::tokens`
/// (lower case, plurals folded); a folder answers to its name, its topic,
/// and each part of its name ("Beef, pork & lamb" answers "pork").
pub(crate) fn match_query(query: &str) -> Option<(Shelf, &'static Folder)> {
    let mut words = crate::search::tokens(query);
    let tail = |word: &String| {
        QUERY_TAILS
            .iter()
            .any(|tail| crate::search::tokens(tail) == [word.as_str()])
    };
    while words.len() > 1 && words.last().is_some_and(tail) {
        words.pop();
    }
    let wanted = words.join(" ");
    if wanted.is_empty() {
        return None;
    }
    let phrase = |text: &str| crate::search::tokens(text).join(" ");
    [Shelf::Dishes, Shelf::Cuisines]
        .into_iter()
        .find_map(|shelf| {
            shelf
                .folders()
                .into_iter()
                .filter(|folder| folder.slug != OTHER_CUISINE.slug)
                .find(|folder| {
                    phrase(folder.name) == wanted
                        || phrase(folder.topic) == wanted
                        || phrase(&folder.slug.replace('-', " ")) == wanted
                        || folder
                            .name
                            .split(['&', ','])
                            .any(|part| phrase(part) == wanted)
                })
                .map(|folder| (shelf, folder))
        })
}

/// The folder prototypes: per dish folder, its example vectors; per
/// cuisine, one vector.
pub(crate) struct Prototypes {
    pub(crate) dishes: Vec<Vec<Vec<f32>>>,
    pub(crate) cuisines: Vec<Vec<f32>>,
}

static PROTOTYPES: OnceCell<Prototypes> = OnceCell::const_new();
/// Set while a background task embeds the prototypes for a page.
static WARMING: AtomicBool = AtomicBool::new(false);

/// The texts embedded for the prototypes, as retrieval queries.
fn prototype_texts() -> (Vec<String>, Vec<String>) {
    let dishes = DISH_FOLDERS
        .iter()
        .flat_map(|folder| {
            folder
                .examples
                .iter()
                .map(move |example| embeddings::query(&format!("{}: {example}", folder.topic)))
        })
        .collect();
    let cuisines = CUISINES
        .iter()
        .map(|folder| embeddings::query(&format!("{} food", folder.topic)))
        .collect();
    (dishes, cuisines)
}

/// Embeds the prototypes on first use; a failure leaves them for the next
/// call to try again.
async fn prototypes(embedder: &Embedder) -> std::result::Result<&'static Prototypes, String> {
    PROTOTYPES
        .get_or_try_init(|| async {
            let (dish_texts, cuisine_texts) = prototype_texts();
            let inputs: Vec<Input> = dish_texts
                .into_iter()
                .chain(cuisine_texts)
                .map(Input::Text)
                .collect();
            let mut vectors = embedder.embed(&inputs).await?.into_iter();
            let mut dishes = Vec::with_capacity(DISH_FOLDERS.len());
            for folder in DISH_FOLDERS {
                let examples: Vec<Vec<f32>> =
                    vectors.by_ref().take(folder.examples.len()).collect();
                if examples.len() != folder.examples.len() {
                    return Err("embedding service returned too few folder vectors".to_owned());
                }
                dishes.push(examples);
            }
            let cuisines: Vec<Vec<f32>> = vectors.collect();
            if cuisines.len() != CUISINES.len() {
                return Err("embedding service returned too few cuisine vectors".to_owned());
            }
            info!("Library folders are ready");
            Ok(Prototypes { dishes, cuisines })
        })
        .await
}

/// Embeds the prototypes unless they are ready (the startup task runs it
/// once the sidecar answers); a failure is left for a page to retry.
pub(crate) async fn warm(embedder: &Embedder) {
    if let Err(error) = prototypes(embedder).await {
        warn!(%error, "Library folder prototypes failed");
    }
}

/// The prototypes once embedded. A page never waits for them: until they
/// are ready it starts one background task to embed them, which a closed
/// page cannot cancel halfway, and files recipes by their pins alone.
fn ready_prototypes(embedder: &Embedder) -> Option<&'static Prototypes> {
    if let Some(prototypes) = PROTOTYPES.get() {
        return Some(prototypes);
    }
    if !WARMING.swap(true, Ordering::AcqRel) {
        let embedder = embedder.clone();
        tokio::spawn(async move {
            warm(&embedder).await;
            WARMING.store(false, Ordering::Release);
        });
    }
    None
}

/// Index of the dish folder a recipe vector belongs in: the folder whose
/// `CLOSEST_EXAMPLES` closest examples score best on average.
pub(crate) fn dish_folder(vector: &[f32], dishes: &[Vec<Vec<f32>>]) -> Option<usize> {
    let scores: Vec<f32> = dishes
        .iter()
        .map(|examples| {
            let mut scores: Vec<f32> = examples
                .iter()
                .map(|example| embeddings::dot(vector, example))
                .collect();
            scores.sort_by(|a, b| b.total_cmp(a));
            let closest = &scores[..scores.len().min(CLOSEST_EXAMPLES)];
            if closest.is_empty() {
                f32::MIN
            } else {
                closest.iter().sum::<f32>() / closest.len() as f32
            }
        })
        .collect();
    best(&scores).map(|(index, _, _)| index)
}

/// Index of the cuisine a recipe vector belongs in, or None (Other) when
/// the best scores under `CUISINE_MIN_SCORE` or within `CUISINE_MARGIN` of
/// the runner-up.
pub(crate) fn cuisine_of(vector: &[f32], cuisines: &[Vec<f32>]) -> Option<usize> {
    let scores: Vec<f32> = cuisines
        .iter()
        .map(|prototype| embeddings::dot(vector, prototype))
        .collect();
    let (index, top, runner_up) = best(&scores)?;
    (top >= CUISINE_MIN_SCORE && top - runner_up >= CUISINE_MARGIN).then_some(index)
}

/// The best index with its score and the runner-up's (or MIN when alone).
fn best(scores: &[f32]) -> Option<(usize, f32, f32)> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
    let first = *order.first()?;
    let second = order.get(1).map_or(f32::MIN, |index| scores[*index]);
    Some((first, scores[first], second))
}

/// The folder slug the shelf files a recipe in: the cook's pin when it
/// names a folder of the shelf, else the embedding's pick, else Not sorted.
pub(crate) fn file_recipe(
    shelf: Shelf,
    pinned: &str,
    vector: Option<&[f32]>,
    prototypes: Option<&Prototypes>,
) -> &'static str {
    if let Some(folder) = shelf.pinnable(pinned) {
        return folder.slug;
    }
    automatic(shelf, vector, prototypes).map_or(UNSORTED.slug, |folder| folder.slug)
}

/// The folder the embedding picks, or None without a vector or prototypes.
fn automatic(
    shelf: Shelf,
    vector: Option<&[f32]>,
    prototypes: Option<&Prototypes>,
) -> Option<&'static Folder> {
    let (vector, prototypes) = (vector?, prototypes?);
    match shelf {
        Shelf::Dishes => dish_folder(vector, &prototypes.dishes).map(|index| &DISH_FOLDERS[index]),
        Shelf::Cuisines => Some(
            cuisine_of(vector, &prototypes.cuisines)
                .map_or(&OTHER_CUISINE, |index| &CUISINES[index]),
        ),
    }
}

/// Vectors and prototypes for filing a user's library: brings the search
/// index up to date first (like a search), and quietly gives up on any
/// embedding failure or prototypes not ready yet, which leaves recipes
/// unsorted rather than failing or holding up the page.
async fn embedding_inputs(
    state: &AppState,
    user_id: &str,
) -> Result<(HashMap<String, Vec<f32>>, Option<&'static Prototypes>)> {
    let Some(embedder) = state.embedder.as_ref() else {
        return Ok((HashMap::new(), None));
    };
    if let Err(error) = crate::search::refresh_user(state, embedder, user_id).await {
        warn!(%error, "Library folders could not refresh the search index");
    }
    let prototypes = ready_prototypes(embedder);
    let vectors = recipes::indexed_recipes(&state.db, user_id)
        .await?
        .into_iter()
        .map(|entry| (entry.id, entry.vector))
        .collect();
    Ok((vectors, prototypes))
}

/// The folder slug of every recipe of the user on one shelf.
pub(crate) async fn shelve(
    state: &AppState,
    user_id: &str,
    shelf: Shelf,
) -> Result<HashMap<String, &'static str>> {
    let pins = recipes::recipe_pins(&state.db, user_id).await?;
    let (vectors, prototypes) = embedding_inputs(state, user_id).await?;
    Ok(pins
        .into_iter()
        .map(|pin| {
            let pinned = match shelf {
                Shelf::Dishes => &pin.folder,
                Shelf::Cuisines => &pin.cuisine,
            };
            let slug = file_recipe(
                shelf,
                pinned,
                vectors.get(&pin.id).map(Vec::as_slice),
                prototypes,
            );
            (pin.id, slug)
        })
        .collect())
}

/// One folder of the Folders or Cuisines view: a stack of tickets on one pin.
pub(crate) struct Stack {
    pub(crate) slug: &'static str,
    pub(crate) name: &'static str,
    pub(crate) count: usize,
    /// The first `STACK_TITLES` titles, A to Z.
    pub(crate) titles: Vec<String>,
    /// How many recipes the top ticket leaves out behind "… N more".
    pub(crate) more: usize,
}

impl Stack {
    /// Papers showing under the top ticket: one per further recipe, up to two.
    pub(crate) fn layers(&self) -> usize {
        self.count.saturating_sub(1).min(2)
    }
}

/// The non-empty folders of a shelf, in the shelf's order with Not sorted
/// yet last, each with its titles filed A to Z.
pub(crate) fn stacks(
    shelf: Shelf,
    filed: &HashMap<String, &'static str>,
    cards: &[LibraryCard],
) -> Vec<Stack> {
    let mut sorted: Vec<&LibraryCard> = cards.iter().collect();
    sorted.sort_by_cached_key(|card| recipes::a_to_z_key(&card.title));
    shelf
        .folders()
        .into_iter()
        .chain([&UNSORTED])
        .filter_map(|folder| {
            let titles: Vec<String> = sorted
                .iter()
                .filter(|card| filed.get(&card.id).copied() == Some(folder.slug))
                .map(|card| card.title.clone())
                .collect();
            (!titles.is_empty()).then(|| Stack {
                slug: folder.slug,
                name: folder.name,
                count: titles.len(),
                more: titles.len().saturating_sub(STACK_TITLES),
                titles: titles.into_iter().take(STACK_TITLES).collect(),
            })
        })
        .collect()
}

/// The folder open on the library page (`/?folder=`).
pub(crate) struct OpenFolder {
    /// "Folders" or "Cuisines".
    pub(crate) shelf: &'static str,
    pub(crate) name: &'static str,
    pub(crate) slug: &'static str,
}

impl OpenFolder {
    pub(crate) fn new(shelf: Shelf, folder: &Folder) -> Self {
        Self {
            shelf: shelf.label(),
            name: folder.name,
            slug: folder.slug,
        }
    }
}

/// One choice of the recipe page's "Filed in" form.
pub(crate) struct FilingOption {
    pub(crate) slug: &'static str,
    pub(crate) name: &'static str,
}

/// The recipe page's folder or cuisine choice.
pub(crate) struct FilingChoice {
    /// The pinned slug, or empty when sorted automatically.
    pub(crate) pinned: &'static str,
    /// The embedding's pick, or empty when it is not known (yet).
    pub(crate) automatic: &'static str,
    pub(crate) options: Vec<FilingOption>,
}

impl FilingChoice {
    fn new(shelf: Shelf, pinned: &str, automatic: Option<&'static Folder>) -> Self {
        Self {
            pinned: shelf.pinnable(pinned).map_or("", |folder| folder.slug),
            automatic: automatic.map_or("", |folder| folder.name),
            options: shelf
                .folders()
                .into_iter()
                .map(|folder| FilingOption {
                    slug: folder.slug,
                    name: folder.name,
                })
                .collect(),
        }
    }

    /// The folder the recipe is in now, or empty when not known.
    pub(crate) fn shown(&self) -> &'static str {
        self.options
            .iter()
            .find(|option| option.slug == self.pinned)
            .map_or(self.automatic, |option| option.name)
    }
}

/// The recipe page's "Filed in" pill: folder and cuisine.
pub(crate) struct Filing {
    pub(crate) folder: FilingChoice,
    pub(crate) cuisine: FilingChoice,
}

impl Filing {
    /// "Desserts & sweets · French", or empty when neither is known.
    pub(crate) fn summary(&self) -> String {
        [self.folder.shown(), self.cuisine.shown()]
            .into_iter()
            .filter(|name| !name.is_empty() && *name != OTHER_CUISINE.name)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// The recipe page's filing. Never waits on the sidecar: the automatic
/// picks show only once the prototypes are ready.
pub(crate) async fn filing(state: &AppState, user_id: &str, recipe_id: &str) -> Result<Filing> {
    let pin = recipes::recipe_pin(&state.db, user_id, recipe_id).await?;
    let prototypes = PROTOTYPES.get().filter(|_| state.embedder.is_some());
    let vector = match prototypes {
        Some(_) => recipes::recipe_vector(&state.db, recipe_id).await?,
        None => None,
    };
    let vector = vector.as_deref();
    Ok(Filing {
        folder: FilingChoice::new(
            Shelf::Dishes,
            &pin.folder,
            automatic(Shelf::Dishes, vector, prototypes),
        ),
        cuisine: FilingChoice::new(
            Shelf::Cuisines,
            &pin.cuisine,
            automatic(Shelf::Cuisines, vector, prototypes),
        ),
    })
}

#[derive(Deserialize)]
pub(crate) struct FilingForm {
    #[serde(default)]
    folder: String,
    #[serde(default)]
    cuisine: String,
}

/// `POST /recipes/{id}/filing`: pins the recipe to a folder and a cuisine
/// (empty sorts it automatically again). The recipe itself is unchanged, so
/// it keeps its `updated_at` and records no version.
pub(crate) async fn update_filing(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<String>,
    Form(form): Form<FilingForm>,
) -> Result<Response> {
    let folder = form.folder.trim();
    let cuisine = form.cuisine.trim();
    if !folder.is_empty() && Shelf::Dishes.pinnable(folder).is_none() {
        return Err(AppError::BadRequest("Unknown folder.".into()));
    }
    if !cuisine.is_empty() && Shelf::Cuisines.pinnable(cuisine).is_none() {
        return Err(AppError::BadRequest("Unknown cuisine.".into()));
    }
    recipes::set_recipe_pin(&state.db, &user.id, &id, folder, cuisine).await?;
    Ok(Redirect::to(&format!("/recipes/{id}")).into_response())
}
