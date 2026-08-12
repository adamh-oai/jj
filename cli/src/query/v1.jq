def __jj_isboolean: . == true or . == false;
def __jj_isnumber: . > true and . < "";
def __jj_isstring: . >= "" and . < [];
def __jj_isarray: . >= [] and . < {};
def __jj_isobject: . >= {};

def type:
    if . == [][0] then "null"
    elif __jj_isboolean then "boolean"
    elif . < "" then "number"
    elif . < [] then "string"
    elif . < {} then "array"
    else "object" end;
def values: select(. != [][0]);
def nulls: select(. == [][0]);
def booleans: select(__jj_isboolean);
def numbers: select(__jj_isnumber);
def strings: select(__jj_isstring);
def arrays: select(__jj_isarray);
def objects: select(__jj_isobject);
def iterables: select(. >= []);
def scalars: select(. < []);
def inside(x): x | contains(.);
def index(s): indices(s) | .[0];
def rindex(s): indices(s) | .[-1];
def add(f): reduce f as $x ([][0]; . + $x);
def add: add(.[]);
def min_by(f): reduce min_by_or_empty(f) as $x ([][0]; $x);
def max_by(f): reduce max_by_or_empty(f) as $x ([][0]; $x);
def min: min_by(.);
def max: max_by(.);
def unique_by(f): [group_by(f)[] | .[0]];
def unique: unique_by(.);
def keys: keys_unsorted | sort;
def flatten: [recurse(arrays[]) | select(__jj_isarray | not)];
def split($separator): . / $separator;
def tonumber: if __jj_isnumber then . else fromjson end;
def @json: tojson;
def @base64: tostring | __jj_encode_base64;
def @uri: tostring | __jj_encode_uri;
def @csv: __jj_format_csv;
def @tsv: __jj_format_tsv;
