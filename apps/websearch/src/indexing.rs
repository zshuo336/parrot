//! Complete-document replacement. Acceptance and durable Flush are distinct.
use super::{checked_reply, confirm_count, enc_doc_meta, enc_texts, put_str, put_u32};
use super::{WsDocumentsAck, WsIndexDocuments, WsTerms, WsTokenize};
use parrot_api::address::ActorRef;
use std::collections::{HashMap, HashSet};

// Leave room for transport paths, reply address and type key below Wire's 16 MiB.
const MAX_PAYLOAD: usize = 15 * 1024 * 1024;

pub(crate) struct Document {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub text: String,
}

fn unique_documents(documents: &[Document]) -> Result<Vec<&Document>, String> {
    let mut positions: HashMap<u64, usize> = HashMap::new();
    let mut unique: Vec<&Document> = Vec::new();
    for doc in documents {
        if let Some(&position) = positions.get(&doc.id) {
            if unique[position].url != doc.url {
                return Err("document identity collision between different URLs".into());
            }
            // Redirect aliases can converge on one final URL in the same wave.
            unique[position] = doc;
        } else {
            positions.insert(doc.id, unique.len());
            unique.push(doc);
        }
    }
    Ok(unique)
}

fn token_groups<'a>(docs: &[&'a Document], limit: usize) -> Result<Vec<Vec<&'a Document>>, String> {
    let mut groups = Vec::new();
    let mut group = Vec::new();
    let mut size = 4usize;
    for &doc in docs {
        let item = doc
            .text
            .len()
            .checked_add(12)
            .ok_or("tokenization size overflow")?;
        if item > limit.saturating_sub(4) {
            return Err("document exceeds tokenization payload budget".into());
        }
        if item > limit.saturating_sub(size) || group.len() == 65_536 {
            groups.push(std::mem::take(&mut group));
            size = 4;
        }
        group.push(doc);
        size += item;
    }
    if !group.is_empty() {
        groups.push(group);
    }
    Ok(groups)
}

fn encode_documents(
    docs: &[&Document],
    entries: Vec<(String, u64, u32)>,
    limit: usize,
) -> Result<Vec<(u32, Vec<u8>)>, String> {
    let ids: HashSet<_> = docs.iter().map(|doc| doc.id).collect();
    let mut terms: HashMap<u64, Vec<(String, u32)>> = HashMap::new();
    let mut seen = HashSet::new();
    if entries.len() > 65_536 {
        return Err("tokenizer reply failed validation: too many terms".into());
    }
    for (term, id, tf) in entries {
        if !ids.contains(&id) || term.is_empty() || tf == 0 || !seen.insert((id, term.clone())) {
            return Err("tokenizer reply failed validation: unknown document, empty/duplicate term or zero frequency".into());
        }
        terms.entry(id).or_default().push((term, tf));
    }
    let mut batches = Vec::new();
    let mut batch = vec![0; 4];
    let mut count = 0u32;
    for doc in docs {
        if doc.url.len() > limit || doc.title.len() > limit {
            return Err("document metadata exceeds payload budget".into());
        }
        let mut bytes = enc_doc_meta(doc.id, &doc.url, &doc.title, &doc.text);
        let words = terms.remove(&doc.id).unwrap_or_default();
        put_u32(&mut bytes, words.len() as u32);
        for (term, tf) in words {
            put_str(&mut bytes, &term);
            put_u32(&mut bytes, tf);
        }
        if bytes.len() > limit.saturating_sub(4) {
            return Err("complete document exceeds indexing payload budget".into());
        }
        if bytes.len() > limit.saturating_sub(batch.len()) || count == 65_536 {
            batch[..4].copy_from_slice(&count.to_le_bytes());
            batches.push((count, std::mem::replace(&mut batch, vec![0; 4])));
            count = 0;
        }
        batch.extend_from_slice(&bytes);
        count += 1;
    }
    if count > 0 {
        batch[..4].copy_from_slice(&count.to_le_bytes());
        batches.push((count, batch));
    }
    Ok(batches)
}

pub(crate) async fn submit(
    tokenizer: &dyn ActorRef,
    searcher: &dyn ActorRef,
    documents: &[Document],
) -> Result<(), String> {
    let unique = unique_documents(documents)?;
    for group in token_groups(&unique, MAX_PAYLOAD)? {
        let texts: Vec<_> = group
            .iter()
            .map(|doc| (doc.id, doc.text.as_str()))
            .collect();
        let WsTerms(entries) = checked_reply(
            tokenizer,
            "tokenize documents",
            Box::new(WsTokenize(enc_texts(&texts))),
        )
        .await?;
        // Validate every returned term before sending any document from this group.
        for (expected, bytes) in encode_documents(&group, entries, MAX_PAYLOAD)? {
            let WsDocumentsAck(actual) = checked_reply(
                searcher,
                "write complete documents",
                Box::new(WsIndexDocuments(bytes)),
            )
            .await?;
            confirm_count("write complete documents", actual, expected..=expected)?;
        }
    }
    // The caller still owes Flush before declaring persistence/task completion.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(id: u64, text: &str) -> Document {
        Document {
            id,
            url: format!("https://example.test/{id}"),
            title: "title".into(),
            text: text.into(),
        }
    }
    #[test]
    fn independent_wire_vector_includes_zero_term_documents() {
        let a = doc(1, "body");
        let b = doc(2, "");
        let encoded =
            encode_documents(&[&a, &b], vec![("word".into(), 1, u32::MAX)], MAX_PAYLOAD).unwrap();
        fn string(b: &mut Vec<u8>, s: &str) {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        let mut expected = 2u32.to_le_bytes().to_vec();
        for d in [&a, &b] {
            expected.extend_from_slice(&d.id.to_le_bytes());
            for field in [&d.url, &d.title, &d.text] {
                string(&mut expected, field);
            }
            expected.extend_from_slice(&(if d.id == 1 { 1u32 } else { 0u32 }).to_le_bytes());
            if d.id == 1 {
                string(&mut expected, "word");
                expected.extend_from_slice(&u32::MAX.to_le_bytes());
            }
        }
        assert_eq!(encoded, vec![(2, expected)]);
    }
    #[test]
    fn aliases_use_the_last_full_document_but_identity_collisions_fail() {
        let a = doc(1, "old");
        let b = doc(1, "new");
        assert_eq!(unique_documents(&[a, b]).unwrap()[0].text, "new");
        let a = doc(1, "old");
        let mut b = doc(1, "new");
        b.url.push_str("/collision");
        assert!(unique_documents(&[a, b]).is_err());
    }
    #[test]
    fn payload_splitting_keeps_each_document_whole_and_in_order() {
        let a = doc(1, "a");
        let b = doc(2, "bb");
        let c = doc(3, "ccc");
        let docs = [&a, &b, &c];
        let groups = token_groups(&docs, 31).unwrap();
        assert_eq!(
            groups
                .iter()
                .map(|g| g.iter().map(|d| d.id).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            vec![vec![1, 2], vec![3]]
        );
        assert!(token_groups(&docs, 18).is_err());
        let bytes = encode_documents(&[&a], vec![], MAX_PAYLOAD).unwrap()[0]
            .1
            .len();
        let groups = encode_documents(&docs, vec![], bytes + 2).unwrap();
        assert_eq!(groups.len(), 3);
        for (index, (count, payload)) in groups.into_iter().enumerate() {
            assert_eq!(count, 1);
            assert!(payload.len() <= bytes + 2);
            assert_eq!(
                u64::from_le_bytes(payload[4..12].try_into().unwrap()),
                index as u64 + 1
            );
        }
        assert!(encode_documents(&[&c], vec![], bytes).is_err());
    }
    #[tokio::test]
    async fn bad_tokenizer_semantics_never_reach_the_index() {
        for entries in [
            vec![("foreign".into(), 2, 1)],
            vec![("zero".into(), 1, 0)],
            vec![(String::new(), 1, 1)],
            vec![("dup".into(), 1, 1), ("dup".into(), 1, 2)],
        ] {
            crate::reply_peer::with_script(
                "bin:ws/Terms",
                vec![crate::enc_terms(&entries)],
                |_client, target| async move {
                    // The peer provides just one tokenization response. It records no
                    // index mutation; validation must fail without a second request.
                    let result = submit(&target, &target, &[doc(1, "body")]).await;
                    assert!(result
                        .unwrap_err()
                        .contains("tokenizer reply failed validation"));
                },
            )
            .await;
        }
    }
}
