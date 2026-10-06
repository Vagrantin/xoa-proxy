# updateinfo.xml (yum/dnf advisories) from stable.json: one <update> per stable entry with an advisory.
def esc: tostring | gsub("&"; "&amp;") | gsub("<"; "&lt;") | gsub(">"; "&gt;") | gsub("\""; "&quot;");
# NEVRA name-version-release.arch, split for the <package> element.
def nevra($n): ($n | capture("^(?<name>.+)-(?<version>[^-]+)-(?<release>[^-]+)\\.(?<arch>[^.]+)$"));
"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<updates>\n" +
([.entries[] | select(.status == "stable" and .advisory != null) | . as $e | $e.advisory as $a |
  "  <update from=\"xcp-hl\" status=\"stable\" type=\"\($a.type | esc)\" version=\"1\">\n" +
  "    <id>\($a.id | esc)</id>\n" +
  "    <title>\($a.summary | esc)</title>\n" +
  "    <severity>\($a.severity // "None" | esc)</severity>\n" +
  "    <issued date=\"\($a.issued | sub("T"; " ") | sub("Z$"; "") | esc)\"/>\n" +
  "    <description>\($a.summary | esc) (fixed in \($e.tag | esc))</description>\n" +
  "    <references>\n" +
  ([($a.cves // [])[] | "      <reference href=\"https://nvd.nist.gov/vuln/detail/\(esc)\" id=\"\(esc)\" type=\"cve\" title=\"\(esc)\"/>\n"] | join("")) +
  "    </references>\n" +
  "    <pkglist>\n      <collection short=\"\($e.tag | esc)\">\n        <name>\($e.tag | esc)</name>\n" +
  ([$e.assets[] | nevra(.nevra) as $p | . as $x |
    "        <package name=\"\($p.name | esc)\" version=\"\($p.version | esc)\" release=\"\($p.release | esc)\" epoch=\"0\" arch=\"\($p.arch | esc)\">\n" +
    "          <filename>\($x.name | esc)</filename>\n          <sum type=\"sha256\">\($x.sha256 | esc)</sum>\n        </package>\n"] | join("")) +
  "      </collection>\n    </pkglist>\n  </update>\n"] | join("")) +
"</updates>"
