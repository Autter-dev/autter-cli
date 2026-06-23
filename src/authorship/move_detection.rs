use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Represents a single inserted line from diff-match-patch output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertedLine {
    pub content: String,
    pub normalized_content: String,
    pub line_number: usize,
    pub insertion_idx: usize,
}

impl InsertedLine {
    pub fn new(content: impl Into<String>, line_number: usize, insertion_idx: usize) -> Self {
        InsertedLine {
            content: content.into(),
            normalized_content: String::new(),
            line_number,
            insertion_idx,
        }
    }
}

/// Represents a single deleted line from diff-match-patch output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedLine {
    pub content: String,
    pub normalized_content: String,
    pub line_number: usize,
    pub deletion_idx: usize,
}

impl DeletedLine {
    pub fn new(content: impl Into<String>, line_number: usize, deletion_idx: usize) -> Self {
        DeletedLine {
            content: content.into(),
            normalized_content: String::new(),
            line_number,
            deletion_idx,
        }
    }
}

/// Mapping for a detected move between deletion and insertion groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveMapping {
    pub deletion_group_index: usize,
    pub insertion_group_index: usize,
    pub deleted: Vec<DeletedLine>,
    pub inserted: Vec<InsertedLine>,
}

/// Detects moved blocks of lines using contiguous matching based on normalized content.
pub fn detect_moves(
    inserted_lines: &mut [InsertedLine],
    deleted_lines: &mut [DeletedLine],
    threshold: usize,
) -> Vec<MoveMapping> {
    if threshold == 0 {
        return Vec::new();
    }

    sort_and_normalize(inserted_lines);
    sort_and_normalize(deleted_lines);

    let threshold = threshold.max(1);
    let inserted_groups = build_groups(inserted_lines, threshold);
    let deleted_groups = build_groups(deleted_lines, threshold);

    if inserted_groups.is_empty() || deleted_groups.is_empty() {
        return Vec::new();
    }

    let deletion_lookup = build_deletion_lookup(deleted_lines, &deleted_groups);
    let mut mappings = Vec::new();

    'insert_groups: for (insert_group_idx, insert_group) in inserted_groups.iter().enumerate() {
        let mut insert_pos = 0;
        while insert_pos < insert_group.len() {
            let inserted_index = insert_group[insert_pos];
            let inserted_line = &inserted_lines[inserted_index];
            let hash = hash_normalized(inserted_line.normalized_content());
            let mut advanced = false;

            if let Some(candidates) = deletion_lookup.get(&hash) {
                for &(delete_group_idx, delete_pos) in candidates {
                    let delete_group = &deleted_groups[delete_group_idx];
                    let delete_index = delete_group[delete_pos];
                    let delete_line = &deleted_lines[delete_index];

                    if inserted_line.normalized_content() != delete_line.normalized_content() {
                        continue;
                    }

                    let mut match_len = 1;
                    let mut insert_iter = insert_pos + 1;
                    let mut delete_iter = delete_pos + 1;

                    while insert_iter < insert_group.len() && delete_iter < delete_group.len() {
                        let insert_idx = insert_group[insert_iter];
                        let delete_idx = delete_group[delete_iter];
                        let insert_line = &inserted_lines[insert_idx];
                        let delete_line = &deleted_lines[delete_idx];

                        if insert_line.normalized_content() != delete_line.normalized_content() {
                            break;
                        }

                        match_len += 1;
                        insert_iter += 1;
                        delete_iter += 1;
                    }

                    if match_len >= threshold {
                        let matched_inserted = insert_group[insert_pos..insert_pos + match_len]
                            .iter()
                            .map(|&idx| inserted_lines[idx].clone())
                            .collect();
                        let matched_deleted = delete_group[delete_pos..delete_pos + match_len]
                            .iter()
                            .map(|&idx| deleted_lines[idx].clone())
                            .collect();

                        mappings.push(MoveMapping {
                            deletion_group_index: delete_group_idx,
                            insertion_group_index: insert_group_idx,
                            deleted: matched_deleted,
                            inserted: matched_inserted,
                        });

                        if insert_iter >= insert_group.len() {
                            continue 'insert_groups;
                        } else {
                            insert_pos = insert_iter;
                            advanced = true;
                            break;
                        }
                    }
                }
            }

            if !advanced {
                insert_pos += 1;
            }
        }
    }

    mappings
}

trait LineRecord {
    fn line_number(&self) -> usize;
    fn content(&self) -> &str;
    fn set_normalized_content(&mut self, normalized: String);
    fn normalized_content(&self) -> &str;
}

impl LineRecord for InsertedLine {
    fn line_number(&self) -> usize {
        self.line_number
    }

    fn content(&self) -> &str {
        &self.content
    }

    fn set_normalized_content(&mut self, normalized: String) {
        self.normalized_content = normalized;
    }

    fn normalized_content(&self) -> &str {
        &self.normalized_content
    }
}

impl LineRecord for DeletedLine {
    fn line_number(&self) -> usize {
        self.line_number
    }

    fn content(&self) -> &str {
        &self.content
    }

    fn set_normalized_content(&mut self, normalized: String) {
        self.normalized_content = normalized;
    }

    fn normalized_content(&self) -> &str {
        &self.normalized_content
    }
}

fn sort_and_normalize<T: LineRecord>(lines: &mut [T]) {
    lines.sort_by_key(|line| line.line_number());
    for line in lines.iter_mut() {
        let normalized = line.content().trim().to_string();
        line.set_normalized_content(normalized);
    }
}

fn build_groups<T: LineRecord>(lines: &[T], threshold: usize) -> Vec<Vec<usize>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut last_number: Option<usize> = None;

    for (idx, line) in lines.iter().enumerate() {
        if line.normalized_content().is_empty() {
            continue;
        }

        match last_number {
            Some(prev) if line.line_number() == prev + 1 => current.push(idx),
            _ => {
                if current.len() >= threshold {
                    groups.push(current);
                }
                current = vec![idx];
            }
        }

        last_number = Some(line.line_number());
    }

    if current.len() >= threshold {
        groups.push(current);
    }

    groups
}

fn build_deletion_lookup(
    deleted_lines: &[DeletedLine],
    deleted_groups: &[Vec<usize>],
) -> HashMap<u64, Vec<(usize, usize)>> {
    let mut lookup: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();

    for (group_idx, group) in deleted_groups.iter().enumerate() {
        for (line_pos, &line_idx) in group.iter().enumerate() {
            let hash = hash_normalized(deleted_lines[line_idx].normalized_content());
            lookup.entry(hash).or_default().push((group_idx, line_pos));
        }
    }

    lookup
}

fn hash_normalized(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inserted_line(line_number: usize, insertion_idx: usize, content: &str) -> InsertedLine {
        InsertedLine::new(content, line_number, insertion_idx)
    }

    fn deleted_line(line_number: usize, deletion_idx: usize, content: &str) -> DeletedLine {
        DeletedLine::new(content, line_number, deletion_idx)
    }

    #[test]
    fn detects_basic_move() {
        let mut inserted = vec![
            inserted_line(10, 0, "fn foo() {"),
            inserted_line(11, 0, "    println!(\"hi\");"),
            inserted_line(12, 0, "}"),
        ];
        let mut deleted = vec![
            deleted_line(1, 0, "fn foo() {"),
            deleted_line(2, 0, "    println!(\"hi\");"),
            deleted_line(3, 0, "}"),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);

        assert_eq!(moves.len(), 1);
        let mapping = &moves[0];
        assert_eq!(mapping.deleted.len(), 3);
        assert_eq!(mapping.inserted.len(), 3);
        assert_eq!(mapping.deleted[0].line_number, 1);
        assert_eq!(mapping.inserted[0].line_number, 10);
        assert_eq!(mapping.inserted[0].normalized_content, "fn foo() {");
        assert_eq!(inserted[0].normalized_content, "fn foo() {");
    }

    #[test]
    fn matches_when_whitespace_differs() {
        let mut inserted = vec![
            inserted_line(20, 1, "    let value = 42; "),
            inserted_line(21, 1, "\treturn value;\t"),
            inserted_line(22, 1, "}"),
        ];
        let mut deleted = vec![
            deleted_line(5, 2, "let value = 42;"),
            deleted_line(6, 2, "return value;"),
            deleted_line(7, 2, "}"),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);
        assert_eq!(moves.len(), 1);
        let mapping = &moves[0];
        assert_eq!(
            mapping
                .inserted
                .iter()
                .map(|l| l.normalized_content.as_str())
                .collect::<Vec<_>>(),
            vec!["let value = 42;", "return value;", "}"]
        );
        assert_eq!(mapping.deleted[1].line_number, 6);
    }

    #[test]
    fn drops_whitespace_only_lines() {
        let mut inserted = vec![
            inserted_line(30, 3, "   "),
            inserted_line(31, 3, "let a = 1;"),
            inserted_line(32, 3, ""),
            inserted_line(33, 3, "let b = 2;"),
        ];
        let mut deleted = vec![
            deleted_line(2, 4, "let a = 1;"),
            deleted_line(3, 4, "let b = 2;"),
            deleted_line(4, 4, "   "),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 2);
        assert!(moves.is_empty());
        assert_eq!(inserted[0].normalized_content, "");
        assert_eq!(inserted[1].normalized_content, "let a = 1;");
    }

    #[test]
    fn filters_groups_below_threshold() {
        let mut inserted = vec![inserted_line(1, 5, "alpha"), inserted_line(2, 5, "beta")];
        let mut deleted = vec![deleted_line(10, 6, "alpha"), deleted_line(11, 6, "beta")];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);
        assert!(moves.is_empty());
    }

    #[test]
    fn detects_multiple_groups() {
        let mut inserted = vec![
            inserted_line(50, 7, "fn a() {"),
            inserted_line(51, 7, "    println!(\"A\");"),
            inserted_line(52, 7, "}"),
            inserted_line(70, 8, "fn b() {"),
            inserted_line(71, 8, "    println!(\"B\");"),
            inserted_line(72, 8, "}"),
        ];
        let mut deleted = vec![
            deleted_line(10, 9, "fn b() {"),
            deleted_line(11, 9, "    println!(\"B\");"),
            deleted_line(12, 9, "}"),
            deleted_line(20, 10, "fn a() {"),
            deleted_line(21, 10, "    println!(\"A\");"),
            deleted_line(22, 10, "}"),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);
        assert_eq!(moves.len(), 2);

        let first = &moves[0];
        assert_eq!(
            first
                .inserted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![50, 51, 52]
        );
        assert_eq!(
            first
                .deleted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![20, 21, 22]
        );

        let second = &moves[1];
        assert_eq!(
            second
                .inserted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![70, 71, 72]
        );
        assert_eq!(
            second
                .deleted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn handles_duplicate_candidates() {
        let mut inserted = vec![
            inserted_line(100, 11, "fn shared() {"),
            inserted_line(101, 11, "    call_one();"),
            inserted_line(102, 11, "}"),
        ];
        let mut deleted = vec![
            deleted_line(5, 12, "fn shared() {"),
            deleted_line(6, 12, "    call_one();"),
            deleted_line(7, 12, "}"),
            deleted_line(20, 13, "fn shared() {"),
            deleted_line(21, 13, "    call_two();"),
            deleted_line(22, 13, "}"),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);
        assert_eq!(moves.len(), 1);
        let mapping = &moves[0];
        assert_eq!(
            mapping
                .deleted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
    }

    #[test]
    fn allows_single_line_moves_with_threshold_one() {
        let mut inserted = vec![inserted_line(200, 14, "single line")];
        let mut deleted = vec![deleted_line(40, 15, "single line")];

        let moves = detect_moves(&mut inserted, &mut deleted, 1);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].inserted[0].insertion_idx, 14);
        assert_eq!(moves[0].deleted[0].deletion_idx, 15);
    }

    #[test]
    fn works_with_unsorted_input() {
        let mut inserted = vec![
            inserted_line(12, 16, "}"),
            inserted_line(10, 16, "fn foo() {"),
            inserted_line(11, 16, "    println!(\"foo\");"),
        ];
        let mut deleted = vec![
            deleted_line(3, 17, "}"),
            deleted_line(1, 17, "fn foo() {"),
            deleted_line(2, 17, "    println!(\"foo\");"),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);
        assert_eq!(moves.len(), 1);
        let mapping = &moves[0];
        assert_eq!(
            mapping
                .inserted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert_eq!(
            mapping
                .deleted
                .iter()
                .map(|l| l.line_number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn no_matches_when_normalized_content_differs() {
        let mut inserted = vec![
            inserted_line(10, 18, "let x = 1;"),
            inserted_line(11, 18, "let y = 2;"),
            inserted_line(12, 18, "let z = 3;"),
        ];
        let mut deleted = vec![
            deleted_line(1, 19, "let x = 1;"),
            deleted_line(2, 19, "let y = 20;"),
            deleted_line(3, 19, "let z = 3;"),
        ];

        let moves = detect_moves(&mut inserted, &mut deleted, 3);
        assert!(moves.is_empty());
    }
}
