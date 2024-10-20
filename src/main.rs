use octocrab::{models, Octocrab};
use serde;
use serde::Deserialize;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::thread::sleep;
use std::time::Duration;
use std::{collections::HashSet, io};

mod cli;
use cli::{args, AppParams, PullRequestQueryType};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const BREAK_LINE: &str = r#"

"#;

#[derive(Debug, Deserialize, Clone, PartialEq)]
enum ItemMergeStatus {
    Merged,
    NotMerged,
    Unknown,
}

#[cfg_attr(test, derive(PartialEq))]
#[derive(Deserialize, Debug, Clone)]
struct Item {
    issue_number: String,
    issue_title: String,
    issue_url: String,
    organization_name: String,
    repository_name: String,
    full_repository_name: String,
    repository_url: String,
    user_login: String,
    user_url: String,
    state: String, // "open", "closed"
    merge_status: ItemMergeStatus,
}

#[cfg_attr(test, derive(PartialEq))]
#[derive(Debug, Clone)]
struct LabelledItem {
    name: String,
    repos: Vec<String>,
    items: Vec<Item>,
}

async fn get_prs(
    octocrab: &Octocrab,
    user: &String,
    date_sign: &String,
    date: &String,
    pr_state_query: &str,
) -> octocrab::Result<octocrab::Page<models::issues::Issue>, octocrab::Error> {
    octocrab
        .search()
        .issues_and_pull_requests(&format!(
            "is:pr author:{} {}:{}{}",
            user.as_str(),
            pr_state_query,
            date_sign.as_str(),
            date.as_str(),
        ))
        .send()
        .await
}

fn format_item(user_login: String, item: &Item) -> String {
    format!(
        "- [{}] [#{}]({}) {} ([@{}])",
        item.full_repository_name, item.issue_number, item.issue_url, item.issue_title, user_login
    )
}

fn format_label(repo: &LabelledItem) -> String {
    format!("## {}", repo.name)
}

async fn get_user_items(octocrab: &Octocrab, app_params: &AppParams) -> Vec<Item> {
    let mut items: Vec<Item> = vec![];

    let query_type = match app_params.query_type {
        cli::PullRequestQueryType::Merged => "merged",
        cli::PullRequestQueryType::Created => "created",
    };

    for user in app_params.users.clone() {
        let mut page = get_prs(
            &octocrab,
            &user,
            &app_params.date_sign,
            &app_params.date,
            query_type,
        )
        .await
        .unwrap();

        loop {
            for issue in &page {
                let url = issue.html_url.to_string();
                let mut repository_url_parts = url.split("/").collect::<Vec<&str>>();
                let path_parts = issue
                    .html_url
                    .path()
                    .split("/")
                    .filter(|x| x.len() > 0)
                    .collect::<Vec<&str>>();

                repository_url_parts.pop(); // id
                repository_url_parts.pop(); // /pulls

                let merge_status = if app_params.query_type == PullRequestQueryType::Merged {
                    ItemMergeStatus::Merged
                } else {
                    ItemMergeStatus::Unknown
                };

                items.push(Item {
                    user_login: issue.user.login.clone(),
                    user_url: issue.user.html_url.to_string(),
                    issue_number: issue.number.to_string(),
                    issue_title: issue.title.clone(),
                    issue_url: url.to_string(),
                    organization_name: path_parts[0].to_string(),
                    repository_name: path_parts[1].to_string(),
                    full_repository_name: format!("{}/{}", path_parts[0], path_parts[1]),
                    repository_url: repository_url_parts.join("/"),
                    state: issue.state.clone(),
                    merge_status,
                });
            }
            page = match octocrab.get_page(&page.next).await.unwrap() {
                Some(next_page) => next_page,
                None => {
                    break;
                }
            }
        }

        // Github API doesn't like requests happening too often.
        // We add a timeout here to help with hitting rate limit
        sleep(Duration::from_secs(1));
    }

    items
}

async fn set_item_merge_status(octocrab: &Octocrab, items: &mut Vec<Item>) -> () {
    for item in items {
        match octocrab
            .pulls(item.organization_name.clone(), item.repository_name.clone())
            .is_merged(item.issue_number.parse::<u64>().unwrap())
            .await
        {
            Ok(is_merged) => {
                if is_merged {
                    item.merge_status = ItemMergeStatus::Merged
                } else {
                    item.merge_status = ItemMergeStatus::NotMerged
                }
            }
            Err(_) => item.merge_status = ItemMergeStatus::Unknown,
        }
    }
}

fn filter_items_by_merge_status(items: Vec<Item>) -> Vec<Item> {
    items
        .into_iter()
        .filter(|item| {
            if item.merge_status == ItemMergeStatus::NotMerged && item.state == "closed" {
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>()
}

fn extract_definitions(items: &Vec<Item>) -> Vec<String> {
    let mut unique_users = HashSet::new();
    let mut unique_repositories = HashSet::new();

    for item in items {
        unique_users.insert(format!("[@{}]: {}", item.user_login, item.user_url));
        unique_repositories.insert(format!(
            "[{}]: {}",
            item.full_repository_name, item.repository_url
        ));
    }

    let mut unique_users = Vec::from_iter(unique_users);
    unique_users.sort();

    let mut unique_repositories = Vec::from_iter(unique_repositories);
    unique_repositories.sort();

    let mut definitions = vec![];

    definitions.append(&mut unique_users);
    definitions.append(&mut unique_repositories);

    definitions
}

async fn initialize_octocrab() -> octocrab::Result<Octocrab> {
    match env::vars().find(|(key, _)| key == "GITHUB_PERSONAL_TOKEN") {
        Some((_key, token)) => Octocrab::builder().personal_token(token).build(),
        None => {
            println!("GITHUB_PERSONAL_TOKEN was not provided.");
            Octocrab::builder().build()
        }
    }
}

fn match_items_with_labels<'a>(
    labelled_items: &'a mut Vec<LabelledItem>,
    items: &Vec<Item>,
) -> (&'a Vec<LabelledItem>, Vec<Item>) {
    let mut unknown_items: Vec<Item> = vec![];

    for item in items {
        let labelled_item = labelled_items
            .into_iter()
            .find(|label| label.repos.contains(&item.full_repository_name));

        match labelled_item {
            Some(labelled_item) => {
                labelled_item.items.push(item.clone());
            }
            None => unknown_items.push(item.clone()),
        }
    }

    (labelled_items, unknown_items)
}

fn format_items(items: &Vec<Item>) -> Vec<String> {
    items
        .into_iter()
        .map(|item| format_item(item.user_login.clone(), &item))
        .collect::<Vec<String>>()
}

fn write_twios_file_contents(
    content: &mut Vec<String>,
    labels: &Vec<LabelledItem>,
    unknown_items: &Vec<Item>,
) {
    for (i, label) in labels.iter().filter(|i| i.items.len() > 0).enumerate() {
        if i > 0 {
            content.push(String::from(""));
        }
        content.push(format_label(&label));
        content.push(String::from(""));
        content.append(&mut format_items(&label.items));
    }

    if unknown_items.len() > 0 {
        content.push(String::from(""));
        content.push(String::from("## Unknown"));
        content.push(String::from(""));
        content.append(&mut format_items(unknown_items));
    }
}

fn write_twios_comment_contents(
    content: &mut Vec<String>,
    app_params: &AppParams,
    unknown_items: &Vec<Item>,
) {
    content.push(String::from(""));

    content.push(format!("- TWIOS_PATH {}", app_params.output_path));
    content.push(format!("- TWIOS_DATE {}", app_params.date));
    content.push("- TWIOS_UNLABELLED".to_string());

    let mut unknown_labels = HashSet::new();
    for item in unknown_items.iter() {
        let label = format!(
            "  - [{}] UNKNOWN @{}",
            item.full_repository_name, item.user_login
        );
        if !unknown_labels.contains(&label) {
            unknown_labels.insert(label.clone());
            content.push(label);
        }
    }

    content.push("".to_string());
    content.push("Change repo category to `EXCLUDED` in order to permantently ignore it from TWIOS from now on.".to_string());
}

async fn fetch_data(
    app_params: &AppParams,
) -> octocrab::Result<(Vec<LabelledItem>, Vec<Item>, Vec<String>)> {
    let octocrab = initialize_octocrab().await?;
    let mut items = get_user_items(&octocrab, &app_params).await;
    items = items
        .into_iter()
        .filter(|item| !app_params.exclude.contains(&item.full_repository_name))
        .collect::<Vec<_>>();
    set_item_merge_status(&octocrab, &mut items).await;
    if app_params.exclude_closed_not_merged
        && app_params.query_type.ne(&PullRequestQueryType::Merged)
    {
        items = filter_items_by_merge_status(items);
    }
    items.sort_by_key(|item| item.full_repository_name.clone());
    let markdown_definitions = extract_definitions(&items);

    let mut labelled_items = app_params
        .labels
        .clone()
        .into_iter()
        .map(|label| LabelledItem {
            name: label.name,
            repos: label.repos,
            items: vec![],
        })
        .collect::<Vec<LabelledItem>>();
    let (labels, unknown_items) = match_items_with_labels(&mut labelled_items, &items);

    Ok((labels.clone().to_vec(), unknown_items, markdown_definitions))
}

#[tokio::main]
async fn main() -> octocrab::Result<()> {
    println!("Using this-week-in-open-source v{}", VERSION);
    println!("");

    let (app_params, file_config) = args();

    match app_params.context {
        cli::CliContext::TWIOS => {
            let (labels, unknown_items, markdown_definitions) = fetch_data(&app_params).await?;
            let mut file = File::create(app_params.file_name()).unwrap();
            let mut file_content: Vec<String> = vec![];
            write_twios_file_contents(&mut file_content, &labels, &unknown_items);

            file.write_all(app_params.header.join("\n").as_bytes())
                .unwrap();
            file.write_all(file_content.join("\n").as_bytes()).unwrap();
            file.write(BREAK_LINE.as_bytes()).unwrap();
            file.write_all(markdown_definitions.join("\n").as_bytes())
                .unwrap();
            println!("");
            println!("Done! :)");
        }
        cli::CliContext::COMMENT => {
            let (_labels, unknown_items, _markdown_definitions) = fetch_data(&app_params).await?;
            let mut comment_content: Vec<String> = vec![];
            write_twios_comment_contents(&mut comment_content, &app_params, &unknown_items);
            let twios_comment = cli::TwiosComment {
                body: app_params.comment_body.clone(),
            };

            let mut output = twios_comment.read();

            cli::write_config_to_file(
                app_params.config_path.clone(),
                &cli::merge_with_file_config(&mut output, file_config.unwrap()),
            )
            .unwrap();
            io::stdout()
                .write_all(comment_content.join("\n").as_bytes())
                .unwrap();
        }
        cli::CliContext::UTILITY => {
            if app_params.dedupe {
                let mut config = file_config
                    .expect("Configuration file doesn't exist")
                    .clone();
                cli::dedupe_file_config(&mut config);
                cli::write_config_to_file(app_params.config_path.clone(), &config)
                    .expect("Couldn't write to file");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items_helper() -> Vec<Item> {
        vec![
            Item {
                issue_number: "63".to_string(),
                issue_title: "Update nan".to_string(),
                issue_url: "https://github.com/atom/keyboard-layout/pull/63".to_string(),
                organization_name: "atom".to_string(),
                repository_name: "keyboard-layout".to_string(),
                full_repository_name: "atom/keyboard-layout".to_string(),
                repository_url: "https://github.com/atom/keyboard-layout".to_string(),
                user_login: "mansona".to_string(),
                user_url: "https://github.com/mansona".to_string(),
                state: "closed".to_string(),
                merge_status: ItemMergeStatus::Unknown,
            },
            Item {
                issue_number: "798".to_string(),
                issue_title: "Ember 4 compatibility".to_string(),
                issue_url: "https://github.com/ember-engines/ember-engines/pull/798".to_string(),
                organization_name: "ember-engines".to_string(),
                repository_name: "ember-engines".to_string(),
                full_repository_name: "ember-engines/ember-engines".to_string(),
                repository_url: "https://github.com/ember-engines/ember-engines".to_string(),
                user_login: "BobrImperator".to_string(),
                user_url: "https://github.com/BobrImperator".to_string(),
                state: "open".to_string(),
                merge_status: ItemMergeStatus::Unknown,
            },
        ]
    }

    fn repo_configs_helper() -> Vec<LabelledItem> {
        vec![LabelledItem {
            name: "Ember".to_string(),
            repos: vec!["ember-engines/ember-engines".to_string()],
            items: vec![],
        }]
    }
    #[test]
    fn it_formats_label() {
        assert_eq!("## Ember", format_label(&repo_configs_helper()[0]));
    }
    #[test]
    fn it_formats_item() {
        assert_eq!(
            "- [atom/keyboard-layout] [#63](https://github.com/atom/keyboard-layout/pull/63) Update nan ([@mansona])",
            format_item("mansona".to_string(), &items_helper()[0])
        );
    }

    #[test]
    fn it_formats_items() {
        let expected = vec![
            "- [atom/keyboard-layout] [#63](https://github.com/atom/keyboard-layout/pull/63) Update nan ([@mansona])",
            "- [ember-engines/ember-engines] [#798](https://github.com/ember-engines/ember-engines/pull/798) Ember 4 compatibility ([@BobrImperator])",
        ];
        assert_eq!(expected, format_items(&items_helper()));
    }

    #[test]
    fn it_extracts_definitions() {
        let expected = vec![
            "[@BobrImperator]: https://github.com/BobrImperator",
            "[@mansona]: https://github.com/mansona",
            "[atom/keyboard-layout]: https://github.com/atom/keyboard-layout",
            "[ember-engines/ember-engines]: https://github.com/ember-engines/ember-engines",
        ];
        assert_eq!(expected, extract_definitions(&items_helper()));
    }

    #[test]
    fn it_matches_items_with_labels() {
        let items = items_helper();
        let atom_keyboard_item = items[0].clone();
        let ember_engines_item = items[1].clone();

        let mut labelled_items = vec![LabelledItem {
            name: "Ember".to_string(),
            repos: vec!["ember-engines/ember-engines".to_string()],
            items: vec![],
        }];

        let labels_result = match_items_with_labels(&mut labelled_items, &items);
        let expected = (
            &vec![LabelledItem {
                name: "Ember".to_string(),
                repos: vec!["ember-engines/ember-engines".to_string()],
                items: vec![ember_engines_item],
            }],
            vec![atom_keyboard_item],
        );

        assert_eq!(expected, labels_result);
    }

    #[test]
    fn it_filters_not_merged_items() {
        let items = vec![
            Item {
                issue_number: "63".to_string(),
                issue_title: "Update nan".to_string(),
                issue_url: "https://github.com/atom/keyboard-layout/pull/63".to_string(),
                organization_name: "atom".to_string(),
                repository_name: "keyboard-layout".to_string(),
                full_repository_name: "atom/keyboard-layout".to_string(),
                repository_url: "https://github.com/atom/keyboard-layout".to_string(),
                user_login: "mansona".to_string(),
                user_url: "https://github.com/mansona".to_string(),
                state: "closed".to_string(),
                merge_status: ItemMergeStatus::NotMerged,
            },
            Item {
                issue_number: "798".to_string(),
                issue_title: "Ember 4 compatibility".to_string(),
                issue_url: "https://github.com/ember-engines/ember-engines/pull/798".to_string(),
                organization_name: "ember-engines".to_string(),
                repository_name: "ember-engines".to_string(),
                full_repository_name: "ember-engines/ember-engines".to_string(),
                repository_url: "https://github.com/ember-engines/ember-engines".to_string(),
                user_login: "BobrImperator".to_string(),
                user_url: "https://github.com/BobrImperator".to_string(),
                state: "open".to_string(),
                merge_status: ItemMergeStatus::Unknown,
            },
        ];
        assert_eq!(vec![items[1].clone()], filter_items_by_merge_status(items))
    }
}
