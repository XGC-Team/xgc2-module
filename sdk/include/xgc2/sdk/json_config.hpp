#pragma once

// The configuration object of an instance as the source of a module's settings: create() and configure() hand
// a module one UTF-8 JSON object, and a key path "a/b" addresses {"a": {"b": ...}}, the spelling of the ROS
// parameter namespace the node versions of the algorithms read. A value of another type than asked for is an
// error, and every leaf that was never read is reported by firstUnusedKey() / rejectUnused(), so a typo or a key
// of another module never leaves a default in place silently.
//
// The only header of the SDK with an outside dependency (jsoncpp).

#include <jsoncpp/json/json.h>
#include <xgc2/module.h>

#include <cstdint>
#include <limits>
#include <memory>
#include <set>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <vector>

namespace xgc2 {
namespace sdk {

class JsonConfig {
   public:
    // Throws std::invalid_argument unless `config` is a JSON object (a missing or empty configuration is the
    // empty object). The text is strict JSON: no comments, no duplicate keys, nothing after the object.
    explicit JsonConfig(const xgc2_config* config) {
        if (config == nullptr || config->json == nullptr || config->length == 0U) {
            root_ = Json::Value(Json::objectValue);
            return;
        }
        Json::CharReaderBuilder builder;
        Json::CharReaderBuilder::strictMode(&builder.settings_);
        const std::unique_ptr<Json::CharReader> reader(builder.newCharReader());
        std::string errors;
        if (!reader->parse(config->json, config->json + config->length, &root_, &errors)) {
            throw std::invalid_argument("the configuration is not JSON: " + errors);
        }
        if (!root_.isObject()) {
            throw std::invalid_argument("the configuration is not a JSON object");
        }
    }

    // True when `key` is present (and then `value` is set); throws std::invalid_argument when its type is not
    // the requested one. Reading a key marks it as used.
    template <typename T>
    typename std::enable_if<std::is_floating_point<T>::value, bool>::type get(
        const std::string& key, T& value) const {
        const Json::Value* node = find(key);
        if (node == nullptr) {
            return false;
        }
        if (!node->isNumeric()) {
            throw typeError(key, "a number");
        }
        value = static_cast<T>(node->asDouble());
        return true;
    }
    template <typename T>
    typename std::enable_if<std::is_integral<T>::value && !std::is_same<T, bool>::value, bool>::type
    get(const std::string& key, T& value) const {
        const Json::Value* node = find(key);
        if (node == nullptr) {
            return false;
        }
        if (!node->isIntegral() || !node->isInt64()) {
            throw typeError(key, "an integer");
        }
        const Json::Int64 wide = node->asInt64();
        const bool in_range =
            wide < 0 ? (std::is_signed<T>::value &&
                        wide >= static_cast<Json::Int64>(std::numeric_limits<T>::lowest()))
                     : static_cast<uint64_t>(wide) <=
                           static_cast<uint64_t>(std::numeric_limits<T>::max());
        if (!in_range) {
            throw typeError(key, "an integer in range");
        }
        value = static_cast<T>(wide);
        return true;
    }
    bool get(const std::string& key, bool& value) const {
        const Json::Value* node = find(key);
        if (node == nullptr) {
            return false;
        }
        if (!node->isBool()) {
            throw typeError(key, "a boolean");
        }
        value = node->asBool();
        return true;
    }
    bool get(const std::string& key, std::string& value) const {
        const Json::Value* node = find(key);
        if (node == nullptr) {
            return false;
        }
        if (!node->isString()) {
            throw typeError(key, "a string");
        }
        value = node->asString();
        return true;
    }
    bool get(const std::string& key, std::vector<double>& value) const {
        const Json::Value* node = find(key);
        if (node == nullptr) {
            return false;
        }
        if (!node->isArray()) {
            throw typeError(key, "an array of numbers");
        }
        value.clear();
        for (const Json::Value& item : *node) {
            if (!item.isNumeric()) {
                throw typeError(key, "an array of numbers");
            }
            value.push_back(item.asDouble());
        }
        return true;
    }

    // Presence checks and structure walks do not mark a key as used.
    bool has(const std::string& key) const {
        return peek(key) != nullptr;
    }
    bool isMapping(const std::string& key) const {
        const Json::Value* node = peek(key);
        return node != nullptr && node->isObject();
    }
    std::vector<std::string> keys(const std::string& key) const {
        const Json::Value* node = peek(key);
        return node != nullptr && node->isObject() ? node->getMemberNames()
                                                   : std::vector<std::string>();
    }

    // The first leaf (array, string, number, boolean or null; sorted by path) that was never read, or an empty
    // string when every leaf was.
    std::string firstUnusedKey() const {
        std::set<std::string> leaves;
        collectLeaves(root_, "", leaves);
        for (const std::string& leaf : leaves) {
            if (used_.count(leaf) == 0U) {
                return leaf;
            }
        }
        return {};
    }

    // Throws std::invalid_argument naming the first key (sorted) that nothing read.
    void rejectUnused() const {
        const std::string leaf = firstUnusedKey();
        if (!leaf.empty()) {
            throw std::invalid_argument("unknown configuration key '" + leaf + "'");
        }
    }

   private:
    static std::invalid_argument typeError(const std::string& key, const char* expected) {
        return std::invalid_argument("configuration key '" + key + "' must be " + expected);
    }

    const Json::Value* peek(const std::string& key) const {
        const Json::Value* node = &root_;
        size_t begin = 0;
        for (;;) {
            const size_t end = key.find('/', begin);
            const std::string part =
                key.substr(begin, end == std::string::npos ? std::string::npos : end - begin);
            if (!node->isObject() || !node->isMember(part)) {
                return nullptr;
            }
            node = &(*node)[part];
            if (end == std::string::npos) {
                return node;
            }
            begin = end + 1;
        }
    }
    const Json::Value* find(const std::string& key) const {
        const Json::Value* node = peek(key);
        if (node != nullptr) {
            used_.insert(key);
        }
        return node;
    }
    static void collectLeaves(const Json::Value& node, const std::string& prefix,
                              std::set<std::string>& leaves) {
        for (const std::string& name : node.getMemberNames()) {
            std::string path = prefix;
            if (!path.empty()) {
                path += '/';
            }
            path += name;
            if (node[name].isObject()) {
                collectLeaves(node[name], path, leaves);
            } else {
                leaves.insert(path);
            }
        }
    }

    Json::Value root_;
    mutable std::set<std::string> used_;
};

}  // namespace sdk
}  // namespace xgc2
