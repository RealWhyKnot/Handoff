BEGIN {
  while ((status = (getline entry < ".github/lint/vendored-paths.txt")) > 0) {
    sub(/\r$/, "", entry)
    gsub(/^[[:space:]]+|[[:space:]]+$/, "", entry)
    if (entry == "") {
      continue
    }
    if (entry ~ /^\/.*[^\/]$/) {
      files[substr(entry, 2)] = 1
      continue
    }
    sub(/\/*$/, "/", entry)
    if (substr(entry, 1, 1) == "/") {
      roots[++root_count] = substr(entry, 2)
    } else {
      names[++name_count] = "/" entry
    }
  }
  if (status < 0) {
    print ".github/lint/vendored-paths.txt is missing. Run Install-Lint again." > "/dev/stderr"
    is_broken = 1
    exit
  }
}

function report(file, line, message) {
  printf "%s:%d: %s\n", file, line, message
  has_findings = 1
}

function is_vendored(path, position) {
  if (path in files) {
    return 1
  }
  for (position = 1; position <= root_count; position++) {
    if (index(path, roots[position]) == 1) {
      return 1
    }
  }
  for (position = 1; position <= name_count; position++) {
    if (index("/" path, names[position]) > 0) {
      return 1
    }
  }
  return 0
}

function check_attribute(text, file, line) {
  if (text ~ /#!?\[[[:space:]]*rustfmt::skip/ || text ~ /#!?\[[[:space:]]*cfg_attr[[:space:]]*\(.*rustfmt::skip/) {
    report(file, line, "remove rustfmt::skip and let rustfmt format it (RS-3)")
  }
}

function check_expect(text, line, message, offset, start) {
  while (match(text, /\.expect\([[:space:]]*"([^"\\]|\\.)*"[[:space:]]*\)/)) {
    start = offset + RSTART
    offset += RSTART + RLENGTH - 1
    message = substr(text, RSTART, RLENGTH)
    text = substr(text, RSTART + RLENGTH)
    if (!production[start]) {
      continue
    }
    sub(/^\.expect\([[:space:]]*"/, "", message)
    sub(/"[[:space:]]*\)$/, "", message)
    if (split(message, words, /[[:space:]]+/) < 4) {
      report(FILENAME, line, "say in the expect message why the value is always there (RS-8)")
    }
  }
}

function raw_size(text, position, cursor, char) {
  cursor = position
  char = substr(text, cursor, 1)
  if (char == "b" || char == "c") {
    cursor++
  }
  if (substr(text, cursor, 1) != "r") {
    return 0
  }
  cursor++
  while (substr(text, cursor, 1) == "#") {
    cursor++
  }
  return substr(text, cursor, 1) == "\"" ? cursor - position + 1 : 0
}

function char_size(text, position, cursor, char) {
  cursor = position + 1
  char = substr(text, cursor, 1)
  if (char == "\\") {
    cursor++
    if (substr(text, cursor, 2) == "u{") {
      cursor += 2
      while (substr(text, cursor, 1) ~ /^[0-9A-Fa-f_]$/) {
        cursor++
      }
      if (substr(text, cursor, 1) != "}") {
        return 0
      }
    } else if (substr(text, cursor, 1) == "x") {
      cursor += 2
    }
  } else if (char == "'" || char == "") {
    return 0
  }
  cursor++
  return substr(text, cursor, 1) == "'" ? cursor - position + 1 : 0
}

function clean_line(text, position, char, pair, size, literal_size) {
  delete code_chars
  for (position = 1; position <= length(text); position++) {
    char = substr(text, position, 1)
    pair = substr(text, position, 2)
    size = 1
    if (block_depth > 0) {
      if (pair == "/*") {
        block_depth++
        size = 2
      } else if (pair == "*/") {
        block_depth--
        size = 2
      }
    } else if (raw_end != "") {
      if (char == "\"" && substr(text, position, length(raw_end)) == raw_end) {
        size = length(raw_end)
        raw_end = ""
      }
    } else if (in_string) {
      if (char == "\\") {
        size = 2
      } else if (char == "\"") {
        in_string = 0
      }
      position += size - 1
      continue
    } else if (pair == "//") {
      break
    } else if (pair == "/*") {
      block_depth = 1
      size = 2
    } else if ((char == "r" || char == "b" || char == "c") && (literal_size = raw_size(text, position)) > 0) {
      size = literal_size
      raw_end = substr(text, position, size)
      sub(/^[bcr]+/, "", raw_end)
      sub(/"$/, "", raw_end)
      raw_end = "\"" raw_end
    } else if (char == "\"") {
      in_string = 1
      continue
    } else if (char == "'" && (literal_size = char_size(text, position)) > 0) {
      size = literal_size
    } else {
      code_chars[position] = char
      continue
    }
    position += size - 1
  }
}

function cfg_value(text, kind, body, level, position, start, char, value, result) {
  gsub(/^[[:space:]]+|[[:space:]]+$/, "", text)
  if (text == "test" || text == "false") {
    return 0
  }
  if (text == "true") {
    return 1
  }
  if (text !~ /^(all|any|not)[[:space:]]*\(/) {
    return -1
  }
  kind = text
  sub(/[[:space:]]*\(.*/, "", kind)
  body = text
  sub(/^[^(]*\(/, "", body)
  sub(/\)[[:space:]]*$/, "", body)
  if (kind == "not") {
    value = cfg_value(body)
    return value < 0 ? -1 : !value
  }
  result = kind == "all"
  start = 1
  for (position = 1; position <= length(body) + 1; position++) {
    char = substr(body, position, 1)
    if (char == "(") {
      level++
    } else if (char == ")") {
      level--
    }
    if ((char == "," && level == 0) || position > length(body)) {
      value = substr(body, start, position - start)
      start = position + 1
      if (value ~ /^[[:space:]]*$/) {
        continue
      }
      value = cfg_value(value)
      if ((kind == "all" && value == 0) || (kind == "any" && value == 1)) {
        return value
      }
      if (value < 0) {
        result = -1
      }
    }
  }
  return result
}

function finish_attribute(predicate) {
  check_attribute(attribute, attribute_file, start)
  predicate = attribute
  if (predicate ~ /^#!?\[[[:space:]]*cfg[[:space:]]*\(/) {
    sub(/^#!?\[[[:space:]]*cfg[[:space:]]*\(/, "", predicate)
    sub(/\)[[:space:]]*\]$/, "", predicate)
    if (cfg_value(predicate) == 0 && test_scope < 0) {
      if (substr(attribute, 2, 1) == "!") {
        test_scope = scope_depth
      } else {
        test_pending = 1
        test_brackets = 0
        test_parens = 0
      }
    }
  }
  attribute = ""
}

function track_test_item(text, position, char) {
  delete production
  for (position = 1; position <= length(text); position++) {
    char = position in code_chars ? code_chars[position] : " "
    if (attribute != "" || (char == "#" && (code_chars[position + 1] == "[" || (code_chars[position + 1] == "!" && code_chars[position + 2] == "[")))) {
      if (attribute == "") {
        attribute_file = FILENAME
        start = FNR
        attribute_depth = 0
      }
      attribute = attribute char
      attribute_depth += (char == "[") - (char == "]")
      if (char == "]" && attribute_depth == 0) {
        finish_attribute()
      }
      continue
    }
    production[position] = test_scope < 0 && !test_pending && position in code_chars
    scope_depth += (char == "{") - (char == "}")
    if (test_pending) {
      test_brackets += (char == "[") - (char == "]")
      test_parens += (char == "(") - (char == ")")
      if (test_brackets == 0 && test_parens == 0) {
        if (char == "{") {
          test_pending = 0
          test_scope = scope_depth
        } else if (char == ";") {
          test_pending = 0
        }
      }
    }
    if (test_scope > scope_depth) {
      test_scope = -1
    }
  }
  if (attribute != "") {
    attribute = attribute " "
  }
}

function check_pattern(text, pattern, line, message, offset, start) {
  while (match(text, pattern)) {
    start = offset + RSTART
    if (code_chars[start] != "" || code_chars[start + 1] != "") {
      report(FILENAME, line, message)
      return
    }
    offset += RSTART + RLENGTH - 1
    text = substr(text, RSTART + RLENGTH)
  }
}

function check_line(text, line) {
  if (!is_test_file) {
    check_expect(text, line)
  }
  check_pattern(text, "(^|[^a-z_])pub[[:space:]]*(\\([[:space:]]*(crate|in[[:space:]][^)]*)[[:space:]]*\\))?[[:space:]]+(const[[:space:]]+)?(async[[:space:]]+)?unsafe[[:space:]]+(extern[[:space:]]*(\"[^\"]*\"[[:space:]]*)?)?fn[[:space:]]", line, "an unsafe fn is private or pub(super); expose a safe fn instead (RS-2)")
  check_pattern(text, "map_err\\([[:space:]]*\\|[[:space:]]*[a-z_][a-z0-9_]*[[:space:]]*\\|[[:space:]]*[a-z_][a-z0-9_]*\\.to_string\\(\\)[[:space:]]*\\)|map_err\\([[:space:]]*\\|[^|]*\\|[[:space:]]*format!\\([[:space:]]*\"\\{[a-z_0-9]*\\}\"[[:space:]]*(,[[:space:]]*[a-z_][a-z0-9_]*[[:space:]]*)?\\)[[:space:]]*\\)|map_err\\([[:space:]]*(std::string::)?ToString::to_string[[:space:]]*\\)", line, "add what was being done to the error text (rust.md §3)")
}

FNR == 1 {
  if (attribute != "") {
    check_attribute(attribute, attribute_file, start)
  }
  attribute = ""
  is_vendored_file = is_vendored(FILENAME)
  is_test_file = FILENAME ~ /(^|\/)(benches|examples)\// || FILENAME ~ /(^|\/)([^\/]*_)?tests?(\/|\.rs$)/
  test_pending = 0
  test_scope = -1
  scope_depth = 0
  test_brackets = 0
  test_parens = 0
  block_depth = 0
  in_string = 0
  raw_end = ""
}

is_vendored_file {
  next
}

{
  clean_line($0)
  track_test_item($0)
  check_line($0, FNR)
}

END {
  if (is_broken) {
    exit 2
  }
  if (attribute != "") {
    check_attribute(attribute, attribute_file, start)
  }
  exit has_findings
}
