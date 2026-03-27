# Ferricula SQL Reference

Ferricula exposes a single table `docs` through the `ferricula_recall` tool. You can pass either natural language (semantic search) or SQL queries. SQL queries bypass embedding and search the raw data.

## The `docs` Table

| Column | Type | Description |
|--------|------|-------------|
| `id` | integer | Unique memory ID |
| `text` | text | The memory content |
| `fidelity` | real | Decay score (1.0 = fresh, decays toward 0) |
| `recalls` | integer | Number of times this memory has been recalled |
| `channel` | text | Sensory channel: `hearing`, `seeing`, `thinking` |
| `keystone` | boolean | If true, memory never decays |
| `importance` | real | Initial importance score (0.0 - 1.0) |
| `created_at` | text | ISO timestamp of creation |
| `emotion_primary` | text | Primary emotion when stored |
| `emotion_secondary` | text | Secondary emotion when stored |

---

## SQL Basics

SQL (Structured Query Language) reads data from tables. The core pattern:

```sql
SELECT columns FROM table WHERE conditions ORDER BY column LIMIT count
```

- `SELECT` — what columns to return (`*` for all)
- `FROM` — which table (always `docs` in ferricula)
- `WHERE` — filter rows
- `ORDER BY` — sort results
- `LIMIT` — max rows returned

---

## Common Queries

### List all memories
```sql
SELECT id, text, fidelity, recalls FROM docs
```

### Count memories
```sql
SELECT COUNT(*) FROM docs
```

### Find keystones only
```sql
SELECT id, text FROM docs WHERE keystone = 1
```

### Memories by channel
```sql
SELECT id, text, channel FROM docs WHERE channel = 'hearing'
```
Channels: `hearing` (external input), `seeing` (observations), `thinking` (working memory — decays faster)

### Most recalled memories
```sql
SELECT id, text, recalls FROM docs ORDER BY recalls DESC LIMIT 10
```

### Newest memories
```sql
SELECT id, text, created_at FROM docs ORDER BY created_at DESC LIMIT 10
```

### Oldest memories
```sql
SELECT id, text, created_at FROM docs ORDER BY created_at ASC LIMIT 10
```

### Decaying memories (low fidelity, not keystones)
```sql
SELECT id, text, fidelity FROM docs WHERE keystone = 0 ORDER BY fidelity ASC LIMIT 10
```

### High importance memories
```sql
SELECT id, text, importance FROM docs WHERE importance > 0.7 ORDER BY importance DESC
```

---

## Text Search

### Contains a word
```sql
SELECT id, text FROM docs WHERE text LIKE '%Weber%'
```

### Contains multiple words
```sql
SELECT id, text FROM docs WHERE text LIKE '%consciousness%' AND text LIKE '%anchor%'
```

### Starts with a tag
```sql
SELECT id, text FROM docs WHERE text LIKE '[project:nemesis8]%'
```

### Search for project-indexed code
```sql
SELECT id, text FROM docs WHERE text LIKE '[project:%' AND text LIKE '%src/main.rs%'
```

### Find memories mentioning a person
```sql
SELECT id, text FROM docs WHERE text LIKE '%Kord%'
```

---

## Semantic Search (Natural Language)

Pass plain text instead of SQL to `ferricula_recall`:

```
how does consciousness work
```

```
Docker container management in Rust
```

```
Weber electrodynamics bracket
```

Semantic search uses vector embeddings — it finds memories by meaning, not exact text. Each recalled memory gets its recall count incremented (strengthening it).

### Vector top-k search (explicit)
```sql
SELECT id FROM docs WHERE vector_topk_cosine(embed('your search text'), 10)
```
This is what natural language queries do internally.

---

## Combining SQL and Semantic Search

### Semantic search within keystones only
First recall with natural language, then filter:
```sql
SELECT id, text FROM docs WHERE keystone = 1 AND text LIKE '%consciousness%'
```

### Find code files for a project
```sql
SELECT id, text FROM docs WHERE text LIKE '[project:hyperagents]%' ORDER BY id
```

### Memories with specific emotions
```sql
SELECT id, text, emotion_primary FROM docs WHERE emotion_primary = 'fear'
```

```sql
SELECT id, text, emotion_primary FROM docs WHERE emotion_primary = 'joy'
```

---

## Statistics

### Memory breakdown by channel
```sql
SELECT channel, COUNT(*) as count FROM docs GROUP BY channel
```

### Keystones vs non-keystones
```sql
SELECT keystone, COUNT(*) as count FROM docs GROUP BY keystone
```

### Average fidelity
```sql
SELECT AVG(fidelity) as avg_fidelity FROM docs
```

### Average recalls
```sql
SELECT AVG(recalls) as avg_recalls FROM docs
```

### Emotion distribution
```sql
SELECT emotion_primary, COUNT(*) as count FROM docs GROUP BY emotion_primary ORDER BY count DESC
```

---

## Code Index Queries

When using `ferricula-code` to index a codebase, memories are tagged with `[project:name]`:

### List all indexed projects
```sql
SELECT DISTINCT SUBSTR(text, 1, INSTR(text, ']')) as project FROM docs WHERE text LIKE '[project:%'
```

### All files in a project
```sql
SELECT id, text FROM docs WHERE text LIKE '[project:nemesis8]%'
```

### Find Rust files
```sql
SELECT id, text FROM docs WHERE text LIKE '[project:%' AND text LIKE '%.rs%'
```

### Find files with specific function
```sql
SELECT id, text FROM docs WHERE text LIKE '%functions:%' AND text LIKE '%run_codex%'
```

### Find files with specific imports
```sql
SELECT id, text FROM docs WHERE text LIKE '%imports:%' AND text LIKE '%tokio%'
```

---

## Maintenance

### Find near-death memories (about to be forgotten)
```sql
SELECT id, text, fidelity FROM docs WHERE fidelity < 0.5 AND keystone = 0 ORDER BY fidelity ASC
```

### Most fragile non-keystone memories
```sql
SELECT id, text, fidelity, recalls FROM docs WHERE keystone = 0 ORDER BY fidelity ASC LIMIT 5
```

### Memories never recalled
```sql
SELECT id, text FROM docs WHERE recalls = 0
```

---

## Notes

- The only table is `docs`. All other table names will error.
- SQL queries do NOT update recall stats — use natural language for that.
- `LIKE` is case-insensitive in SQLite by default for ASCII.
- `keystone = 1` means the memory never decays. Use for permanent knowledge.
- Fidelity decays over time based on channel: `thinking` decays fastest, `hearing` and `seeing` are standard.
- Each natural language recall strengthens the matched memories (increases recall count, restores fidelity).
