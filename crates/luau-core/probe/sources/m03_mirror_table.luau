local function mt_empty(k) local box = {} box[k] = k return box end
local function mt_named() return { first = 4, second = 5 } end
local function mt_array(p, q, r) return { p, q, r } end
local function mt_field(box) box.left = box.right return box end
local function mt_index(box, k) return box[k] end
local function mt_slot(box) box[5] = box[4] return box end
local function mt_both(p) return p and 6 end
local function mt_either(p) return p or 6 end
local function mt_andreg(p, q) return p and q end
local function mt_orreg(p, q) return p or q end
return { mt_empty, mt_named, mt_array, mt_field, mt_index, mt_slot, mt_both, mt_either, mt_andreg, mt_orreg }
