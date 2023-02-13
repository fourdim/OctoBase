use super::{Content, PluginImpl};
use lib0::any::Any;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{atomic::AtomicU32, Arc};
use tantivy::{collector::TopDocs, query::QueryParser, schema::*, Index, ReloadPolicy};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SearchResult {
    pub block_id: String,
    pub score: f32,
}

/// Returned from [`Workspace::search`]
///
/// [`Workspace::search`]: crate::Workspace::search
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SearchResults(Vec<SearchResult>);

pub struct IndexingPluginImpl {
    // /// `true` if the text search has not yet populated the Tantivy index
    // /// `false` if there should only be incremental changes necessary to the blocks.
    // first_index: bool,
    pub(super) queue_reindex: Arc<AtomicU32>,
    pub(super) schema: Schema,
    pub(super) index: Rc<Index>,
    pub(super) query_parser: QueryParser,
    // need to keep so it gets dropped with this plugin
    pub(super) _update_sub: yrs::Subscription<yrs::UpdateEvent>,
}

impl IndexingPluginImpl {
    pub fn search<S: AsRef<str>>(
        &self,
        query: S,
    ) -> Result<SearchResults, Box<dyn std::error::Error>> {
        let mut items = Vec::new();

        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommit)
            .try_into()?;
        let searcher = reader.searcher();
        let query = self.query_parser.parse_query(query.as_ref())?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(10))?;
        // The actual documents still need to be retrieved from Tantivy’s store.
        // Since the body field was not configured as stored, the document returned will only contain a title.

        if !top_docs.is_empty() {
            let block_id_field = self.schema.get_field("block_id").unwrap();

            for (score, doc_address) in top_docs {
                let retrieved_doc = searcher.doc(doc_address)?;
                if let Some(Value::Str(id)) = retrieved_doc.get_first(block_id_field) {
                    items.push(SearchResult {
                        block_id: id.to_string(),
                        score,
                    });
                } else {
                    let to_json = self.schema.to_json(&retrieved_doc);
                    eprintln!("Unexpected non-block doc in Tantivy result set: {to_json}");
                }
            }
        }

        Ok(SearchResults(items))
    }
}

impl PluginImpl for IndexingPluginImpl {
    fn on_update(&mut self, ws: &Content) -> Result<(), Box<dyn std::error::Error>> {
        let curr = self.queue_reindex.load(std::sync::atomic::Ordering::SeqCst);
        if curr > 0 {
            let mut re_index_list = HashMap::<String, (Option<String>, Option<String>)>::new();
            // TODO: reindex
            for block in ws.block_iter() {
                let title = block.content().get("title").map(ToOwned::to_owned);
                let body = block.content().get("text").map(ToOwned::to_owned);
                re_index_list.insert(
                    block.id(),
                    (
                        title.and_then(|a| match a {
                            Any::String(str) => Some(str.to_string()),
                            _ => None,
                        }),
                        body.and_then(|a| match a {
                            Any::String(str) => Some(str.to_string()),
                            _ => None,
                        }),
                    ),
                );
            }

            // dbg!((txn, upd));
            // println!("got update event: {items}");
            // just re-index stupidly
            self.re_index_content(re_index_list)
                .map_err(|err| format!("Error during reindex: {err:?}"))?;
        }

        // reset back down now that the update was applied
        self.queue_reindex
            .fetch_sub(curr, std::sync::atomic::Ordering::SeqCst);

        Ok(())
    }
}

impl IndexingPluginImpl {
    fn re_index_content<BlockIdTitleAndTextIter>(
        &mut self,
        blocks: BlockIdTitleAndTextIter,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        // TODO: use a structure with better names than tuples?
        BlockIdTitleAndTextIter: IntoIterator<Item = (String, (Option<String>, Option<String>))>,
    {
        let block_id_field = self.schema.get_field("block_id").unwrap();
        let title_field = self.schema.get_field("title").unwrap();
        let body_field = self.schema.get_field("body").unwrap();

        let mut writer = self
            .index
            .writer(50_000_000)
            .map_err(|err| format!("Error creating writer: {err:?}"))?;

        for (block_id, (block_title_opt, block_text_opt)) in blocks {
            let mut block_doc = Document::new();
            block_doc.add_text(block_id_field, block_id);
            if let Some(block_title) = block_title_opt {
                block_doc.add_text(title_field, block_title);
            }
            if let Some(block_text) = block_text_opt {
                block_doc.add_text(body_field, block_text);
            }
            writer.add_document(block_doc)?;
        }

        // If .commit() returns correctly, then all of the documents that have been added
        // are guaranteed to be persistently indexed.
        writer.commit()?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::super::*;
    use super::*;

    // out of order for now, in the future, this can be made in order by sorting before
    // we reduce to just the block ids. Then maybe we could first sort on score, then sort on
    // block id.
    macro_rules! expect_result_ids {
        ($search_results:ident, $id_str_array:expr) => {
            let mut sorted_ids = $search_results
                .0
                .iter()
                .map(|i| &i.block_id)
                .collect::<Vec<_>>();
            sorted_ids.sort();
            assert_eq!(
                sorted_ids, $id_str_array,
                "Expected found ids (left) match expected ids (right) for search results"
            );
        };
    }
    macro_rules! expect_search_gives_ids {
        ($search_plugin:ident, $query_text:expr, $id_str_array:expr) => {
            let search_result = $search_plugin
                .search($query_text)
                .expect("no error searching");

            let line = line!();
            println!("Search results (workspace.rs:{line}): {search_result:#?}"); // will show if there is an issue running the test

            expect_result_ids!(search_result, $id_str_array);
        };
    }

    #[test]
    fn basic_search_test() {
        let mut workspace = {
            let workspace = Workspace::from_doc(Default::default(), "wk-load");
            // even though the plugin is added by default,
            super::super::super::insert_plugin(workspace, IndexingPluginRegister::ram())
                .expect("failed to insert plugin")
        };

        workspace.with_trx(|mut t| {
            let block = t.create("b1", "text");
            block.set(&mut t.trx, "test", "test");

            let block = t.create("a", "affine:text");
            let b = t.create("b", "affine:text");
            let c = t.create("c", "affine:text");
            let d = t.create("d", "affine:text");
            let e = t.create("e", "affine:text");
            let f = t.create("f", "affine:text");
            let trx = &mut t.trx;

            b.set(trx, "title", "Title B content");
            b.set(trx, "text", "Text B content bbb xxx");

            c.set(trx, "title", "Title C content");
            c.set(trx, "text", "Text C content ccc xxx yyy");

            d.set(trx, "title", "Title D content");
            d.set(trx, "text", "Text D content ddd yyy");
      
            e.set(trx, "title", "人民日报");
            e.set(trx, "text", "张华考上了北京大学；李萍进了中等技术学校；我在百货公司当售货员：我们都有光明的前途");

            f.set(trx, "title", "美国首次成功在核聚变反应中实现“净能量增益”");
            f.set(trx, "text", "当地时间13日，美国能源部官员宣布，由美国政府资助的加州劳伦斯·利弗莫尔国家实验室（LLNL），首次成功在核聚变反应中实现“净能量增益”，即聚变反应产生的能量大于促发该反应的镭射能量。");

            // pushing blocks in
            {
                block.push_children(trx, &b);
                block.insert_children_at(trx, &c, 0);
                block.insert_children_before(trx, &d, "b");
                block.insert_children_after(trx, &e, "b");
                block.insert_children_after(trx, &f, "c");

                assert_eq!(
                    block.children(),
                    vec![
                        "c".to_owned(),
                        "f".to_owned(),
                        "d".to_owned(),
                        "b".to_owned(),
                        "e".to_owned()
                    ]
                );
            }

            // Question: Is this supposed to indicate that since this block is detached, then we should not be indexing it?
            // For example, should we walk up the parent tree to check if each block is actually attached?
            block.remove_children(trx, &d);
        });

        println!("Blocks: {:#?}", workspace.blocks()); // shown if there is an issue running the test.

        workspace
            .update_plugin::<IndexingPluginImpl>()
            .expect("update text search plugin");

        let search_plugin = workspace.get_plugin::<IndexingPluginImpl>().unwrap();

        expect_search_gives_ids!(search_plugin, "content", &["b", "c", "d"]);
        expect_search_gives_ids!(search_plugin, "bbb", &["b"]);
        expect_search_gives_ids!(search_plugin, "ccc", &["c"]);
        expect_search_gives_ids!(search_plugin, "xxx", &["b", "c"]);
        expect_search_gives_ids!(search_plugin, "yyy", &["c", "d"]);

        expect_search_gives_ids!(search_plugin, "人民日报", &["e"]);
        expect_search_gives_ids!(search_plugin, "技术学校", &["e"]);

        expect_search_gives_ids!(search_plugin, "核聚变反应", &["f"]);
        expect_search_gives_ids!(search_plugin, "镭射能量", &["f"]);
    }
}