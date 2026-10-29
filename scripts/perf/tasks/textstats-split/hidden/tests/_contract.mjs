export const EXPECTED_EXPORTS = [
  'DEFAULT_STOPWORDS',
  'formatSummary',
  'formatTable',
  'splitSentences',
  'summarize',
  'tokenize',
  'topWords',
  'wordFrequency',
].sort();

export const SPLIT_MODULES = {
  'tokenize.js': ['splitSentences', 'tokenize'],
  'stats.js': ['DEFAULT_STOPWORDS', 'summarize', 'topWords', 'wordFrequency'],
  'format.js': ['formatSummary', 'formatTable'],
};
