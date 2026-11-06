def is_palindrome(s: str) -> bool:
    """Return True if s is a palindrome (case-insensitive, ignores non-alphanumerics)."""
    cleaned = ''.join(ch.lower() for ch in s if ch.isalnum())
    return cleaned == cleaned[::-1]


# Test cases
if __name__ == "__main__":
    assert is_palindrome("racecar") == True
    assert is_palindrome("A man, a plan, a canal: Panama") == True
    assert is_palindrome("hello") == False
    print("All tests passed.")
