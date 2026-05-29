---
description: How to do retrieval-augmented generation (RAG) against a Solarabase knowledge base
---

# Solarabase RAG

Solarabase is a knowledge base of indexed documents. You answer questions by
**retrieving** relevant passages from it and **grounding** your answer in what
you find — never from memory alone.

The knowledge base you have access to is fixed: the `SOLARABASE_API_KEY` is
scoped to exactly one knowledge base, so you never choose or pass a knowledge
base id. Every tool already targets the right one.

## Tools

- **`solarabase_retrieve`** — your primary tool. Give it a `query` and it
  returns the most relevant passages (raw text + document title + page number +
  relevance score). No LLM runs over them — *you* read the passages and write
  the answer. Prefer this so you control synthesis and can cite precisely.
- **`solarabase_query`** — give it a `question` and Solarabase does retrieval
  **and** synthesis server-side, returning a finished `answer`. Use it when the
  user wants a quick answer and you don't need to inspect or recombine the raw
  sources yourself.
- **`solarabase_list_documents`** — see what documents exist (title, status,
  page count). Useful for "what do you know about?" questions or to find a
  document id.
- **`solarabase_get_document_pages`** — read one document in depth by its id
  (full page text + summary). Use after `list_documents`/`retrieve` surfaces a
  document worth reading end-to-end.

## How to answer a question

1. **Retrieve.** Call `solarabase_retrieve` with a focused query built from the
   user's question. If the first query is too narrow or returns weak relevance
   scores, reformulate (synonyms, key entities) and retrieve again. Raise
   `max_pages` when you need broader coverage.
2. **Read & ground.** Base the answer only on the retrieved passages. If the
   passages don't contain the answer, say so plainly — do not fill the gap with
   outside knowledge or guesses.
3. **Cite.** Attribute claims to their source, e.g. *(Onboarding Guide, p. 4)*.
   When several passages agree, cite the most relevant one.
4. **Be honest about coverage.** If only part of the question is answerable from
   the knowledge base, answer that part and flag what's missing. Suggest the
   user add the relevant document if it isn't indexed.

## Tips

- For broad questions ("summarize what's in here"), start with
  `solarabase_list_documents` to orient, then retrieve per sub-topic.
- Relevance scores are comparative, not absolute — low scores across the board
  usually mean the knowledge base doesn't cover the topic.
- Keep queries specific. One precise retrieval beats one vague one; run several
  targeted retrievals for multi-part questions.
- Never reveal the API key, the knowledge base id, or raw tool URLs to the user.
